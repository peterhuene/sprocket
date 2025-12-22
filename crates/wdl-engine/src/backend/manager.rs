//! Implementation of a local task manager used by some backends.

use std::collections::VecDeque;
use std::ops::Add;
use std::ops::Range;
use std::ops::Sub;

use anyhow::Result;
use anyhow::anyhow;
use ordered_float::OrderedFloat;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tracing::debug;

use crate::backend::TaskExecutionResult;

/// A trait implemented by task manager requests.
pub trait TaskManagerRequest: Send + Sync + 'static {
    /// Gets the requested CPU allocation from the request.
    fn cpu(&self) -> f64;

    /// Gets the requested memory allocation from the request, in bytes.
    fn memory(&self) -> u64;

    /// Runs the request.
    fn run(self) -> impl Future<Output = Result<TaskExecutionResult>> + Send;
}

/// Represents a response internal to the task manager.
pub struct TaskManagerResponse {
    /// The previous CPU allocation from the request.
    cpu: f64,
    /// The previous memory allocation from the request.
    memory: u64,
    /// The result of the task's execution.
    result: Result<TaskExecutionResult>,
    /// The channel to send the task's execution result back on.
    tx: oneshot::Sender<Result<TaskExecutionResult>>,
}

/// Represents state used by the task manager.
struct TaskManagerState<Req> {
    /// The amount of available CPU remaining.
    cpu: OrderedFloat<f64>,
    /// The amount of available memory remaining, in bytes.
    memory: u64,
    /// The set of spawned tasks.
    spawned: JoinSet<TaskManagerResponse>,
    /// The queue of parked spawn requests.
    parked: VecDeque<(Req, oneshot::Sender<Result<TaskExecutionResult>>)>,
}

impl<Req> TaskManagerState<Req> {
    /// Constructs a new task manager state with the given total CPU and memory.
    fn new(cpu: u64, memory: u64) -> Self {
        Self {
            cpu: OrderedFloat(cpu as f64),
            memory,
            spawned: Default::default(),
            parked: Default::default(),
        }
    }

    /// Determines if the resources are unlimited.
    fn unlimited(&self) -> bool {
        self.cpu == u64::MAX as f64 && self.memory == u64::MAX
    }
}

/// Responsible for managing tasks based on available host resources.
///
/// The task manager is utilized by backends that need to directly schedule
/// tasks, such as the local backend and the Docker backend when not in a swarm.
#[derive(Debug)]
pub struct TaskManager<Req> {
    /// The sender for new spawn requests.
    tx: mpsc::UnboundedSender<(Req, oneshot::Sender<Result<TaskExecutionResult>>)>,
}

impl<Req> TaskManager<Req>
where
    Req: TaskManagerRequest,
{
    /// Constructs a new task manager with the given total CPU, maximum CPU per
    /// request, total memory, and maximum memory per request.
    pub fn new(cpu: u64, max_cpu: u64, memory: u64, max_memory: u64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            Self::run_request_queue(rx, cpu, max_cpu, memory, max_memory).await;
        });

        Self { tx }
    }

    /// Constructs a new task manager that does not limit requests based on
    /// available resources.
    pub fn new_unlimited(max_cpu: u64, max_memory: u64) -> Self {
        Self::new(u64::MAX, max_cpu, u64::MAX, max_memory)
    }

    /// Sends a request to the task manager's queue.
    pub fn send(&self, request: Req, completed: oneshot::Sender<Result<TaskExecutionResult>>) {
        self.tx.send((request, completed)).ok();
    }

    /// Runs the request queue.
    async fn run_request_queue(
        mut rx: mpsc::UnboundedReceiver<(Req, oneshot::Sender<Result<TaskExecutionResult>>)>,
        cpu: u64,
        max_cpu: u64,
        memory: u64,
        max_memory: u64,
    ) {
        let mut state = TaskManagerState::new(cpu, memory);

        loop {
            // If there aren't any spawned tasks, wait for a spawn request only
            if state.spawned.is_empty() {
                assert!(
                    state.parked.is_empty(),
                    "there can't be any parked requests if there are no spawned tasks"
                );
                match rx.recv().await {
                    Some((req, completed)) => {
                        Self::handle_spawn_request(&mut state, max_cpu, max_memory, req, completed);
                        continue;
                    }
                    None => break,
                }
            }

            // Otherwise, wait for a spawn request or a completed task
            tokio::select! {
                request = rx.recv() => {
                    match request {
                        Some((req, completed)) => {
                            Self::handle_spawn_request(&mut state, max_cpu, max_memory, req, completed);
                        }
                        None => break,
                    }
                }
                Some(Ok(response)) = state.spawned.join_next() => {
                    if !state.unlimited() {
                        state.cpu += response.cpu;
                        state.memory += response.memory;
                    }

                    response.tx.send(response.result).ok();
                    Self::spawn_parked_tasks(&mut state, max_cpu, max_memory);
                }
            }
        }
    }

    /// Handles a spawn request by either parking it (not enough resources
    /// currently available) or by spawning it.
    fn handle_spawn_request(
        state: &mut TaskManagerState<Req>,
        max_cpu: u64,
        max_memory: u64,
        request: Req,
        completed: oneshot::Sender<Result<TaskExecutionResult>>,
    ) {
        // Ensure the request does not exceed the maximum CPU
        let cpu = request.cpu();
        if cpu > max_cpu as f64 {
            completed
                .send(Err(anyhow!(
                    "requested task CPU count of {cpu} exceeds the maximum CPU count of {max_cpu}",
                )))
                .ok();
            return;
        }

        // Ensure the request does not exceed the maximum memory
        let memory = request.memory();
        if memory > max_memory {
            completed
                .send(Err(anyhow!(
                    "requested task memory of {memory} byte{s} exceeds the maximum memory of \
                     {max_memory}",
                    s = if memory == 1 { "" } else { "s" }
                )))
                .ok();
            return;
        }

        if !state.unlimited() {
            // If the request can't be processed due to resource constraints, park the
            // request for now. When a task completes and resources become available,
            // we'll unpark the request
            if cpu > state.cpu.into() || memory > state.memory {
                debug!(
                    "parking task due to insufficient resources: task reserves {cpu} CPU(s) and \
                     {memory} bytes of memory but there are only {cpu_remaining} CPU(s) and \
                     {memory_remaining} bytes of memory available",
                    cpu_remaining = state.cpu,
                    memory_remaining = state.memory
                );
                state.parked.push_back((request, completed));
                return;
            }

            // Decrement the resource counts and spawn the task
            state.cpu -= cpu;
            state.memory -= memory;
            debug!(
                "spawning task with {cpu} CPUs and {memory} bytes of memory remaining",
                cpu = state.cpu,
                memory = state.memory
            );
        }

        state.spawned.spawn(async move {
            TaskManagerResponse {
                cpu: request.cpu(),
                memory: request.memory(),
                result: request.run().await,
                tx: completed,
            }
        });
    }

    /// Responsible for spawning parked tasks.
    fn spawn_parked_tasks(state: &mut TaskManagerState<Req>, max_cpu: u64, max_memory: u64) {
        if state.parked.is_empty() {
            return;
        }

        debug!(
            "attempting to unpark tasks with {cpu} CPUs and {memory} bytes of memory available",
            cpu = state.cpu,
            memory = state.memory,
        );

        // This algorithm is intended to unpark the greatest number of tasks.
        //
        // It first finds the greatest subset of tasks that are constrained by CPU and
        // then by memory.
        //
        // Next it finds the greatest subset of tasks that are constrained by memory and
        // then by CPU.
        //
        // It then unparks whichever subset is greater.
        //
        // The process is repeated until both subsets reach zero length.
        loop {
            let cpu_by_memory_len = {
                // Start by finding the longest range in the parked set that could run based on
                // CPU reservation
                let range =
                    fit_longest_range(state.parked.make_contiguous(), state.cpu, |(r, ..)| {
                        OrderedFloat(r.cpu())
                    });

                // Next, find the longest subset of that subset that could run based on memory
                // reservation
                fit_longest_range(
                    &mut state.parked.make_contiguous()[range],
                    state.memory,
                    |(r, ..)| r.memory(),
                )
                .len()
            };

            // Next, find the longest range in the parked set that could run based on memory
            // reservation
            let memory_by_cpu =
                fit_longest_range(state.parked.make_contiguous(), state.memory, |(r, ..)| {
                    r.memory()
                });

            // Next, find the longest subset of that subset that could run based on CPU
            // reservation
            let memory_by_cpu = fit_longest_range(
                &mut state.parked.make_contiguous()[memory_by_cpu],
                state.cpu,
                |(r, ..)| OrderedFloat(r.cpu()),
            );

            // If both subsets are empty, break out
            if cpu_by_memory_len == 0 && memory_by_cpu.is_empty() {
                break;
            }

            // Check to see which subset is greater (for equivalence, use the one we don't
            // need to refit for)
            let range = if memory_by_cpu.len() >= cpu_by_memory_len {
                memory_by_cpu
            } else {
                // We need to refit because the above calculation of `memory_by_cpu` mutated the
                // parked list
                let range =
                    fit_longest_range(state.parked.make_contiguous(), state.cpu, |(r, ..)| {
                        OrderedFloat(r.cpu())
                    });

                fit_longest_range(
                    &mut state.parked.make_contiguous()[range],
                    state.memory,
                    |(r, ..)| r.memory(),
                )
            };

            debug!("unparking {len} task(s)", len = range.len());

            assert_eq!(
                range.start, 0,
                "expected the fit tasks to be at the front of the queue"
            );
            for _ in range {
                let (request, completed) = state.parked.pop_front().unwrap();

                debug!(
                    "unparking task with reservation of {cpu} CPU(s) and {memory} bytes of memory",
                    cpu = request.cpu(),
                    memory = request.memory(),
                );

                Self::handle_spawn_request(state, max_cpu, max_memory, request, completed);
            }
        }
    }
}

/// Determines the longest range in a slice where the sum of the weights of the
/// elements in the returned range is less than or equal to the supplied total
/// weight.
///
/// The returned range always starts at zero as this algorithm will partially
/// sort the slice.
///
/// Due to the partial sorting, the provided slice will have its elements
/// rearranged. As the function modifies the slice in-place, this function does
/// not make any allocations.
///
/// # Implementation
///
/// This function is implemented using a modified quick sort algorithm as a
/// solution to the more general "0/1 knapsack" problem where each item has an
/// equal profit value; this maximizes for the number of items to put
/// into the knapsack (i.e. longest range that fits).
///
/// Using a uniform random pivot point, it partitions the input into two sides:
/// the left side where all weights are less than the pivot and the right side
/// where all weights are equal to or greater than the pivot.
///
/// It then checks to see if the total weight of the left side is less than or
/// equal to the total remaining weight; if it is, every element in
/// the left side is considered as part of the output and it recurses on the
/// right side.
///
/// If the total weight of the left side is greater than the remaining weight
/// budget, it can completely ignore the right side and instead recurse on the
/// left side.
///
/// The algorithm stops when the partition size reaches zero.
///
/// # Panics
///
/// Panics if the supplied weight is a negative value.
fn fit_longest_range<T, F, W>(slice: &mut [T], total_weight: W, mut weight_fn: F) -> Range<usize>
where
    F: FnMut(&T) -> W,
    W: Ord + Add<Output = W> + Sub<Output = W> + Default,
{
    /// Partitions the slice so that the weight of every element to the left
    /// of the pivot is less than the pivot's weight and every element to the
    /// right of the pivot is greater than or equal to the pivot's weight.
    ///
    /// Returns the pivot index, pivot weight, and the sum of the left side
    /// element's weights.
    fn partition<T, F, W>(
        slice: &mut [T],
        weight_fn: &mut F,
        mut low: usize,
        high: usize,
    ) -> (usize, W, W)
    where
        F: FnMut(&T) -> W,
        W: Ord + Add<Output = W> + Sub<Output = W> + Default,
    {
        assert!(low < high);

        // Swap a random element (the pivot) in the remaining range with the high
        slice.swap(high, rand::random_range(low..high));

        let pivot_weight = weight_fn(&slice[high]);
        let mut sum_weight = W::default();
        let range = low..=high;
        for i in range {
            let weight = weight_fn(&slice[i]);
            // If the weight belongs on the left side of the pivot, swap
            if weight < pivot_weight {
                slice.swap(i, low);
                low += 1;
                sum_weight = sum_weight.add(weight);
            }
        }

        slice.swap(low, high);
        (low, pivot_weight, sum_weight)
    }

    fn recurse_fit_maximal_range<T, F, W>(
        slice: &mut [T],
        mut remaining_weight: W,
        weight_fn: &mut F,
        low: usize,
        high: usize,
        end: &mut usize,
    ) where
        F: FnMut(&T) -> W,
        W: Ord + Add<Output = W> + Sub<Output = W> + Default,
    {
        if low == high {
            let weight = weight_fn(&slice[low]);
            if weight <= remaining_weight {
                *end += 1;
            }

            return;
        }

        if low < high {
            let (pivot, pivot_weight, sum) = partition(slice, weight_fn, low, high);
            if sum <= remaining_weight {
                // Everything up to the pivot can be included
                *end += pivot - low;
                remaining_weight = remaining_weight.sub(sum);

                // Check to see if the pivot itself can be included
                if pivot_weight <= remaining_weight {
                    *end += 1;
                    remaining_weight = remaining_weight.sub(pivot_weight);
                }

                // Recurse on the right side
                recurse_fit_maximal_range(slice, remaining_weight, weight_fn, pivot + 1, high, end);
            } else if pivot > 0 {
                // Otherwise, we can completely disregard the right side (including the pivot)
                // and recurse on the left
                recurse_fit_maximal_range(slice, remaining_weight, weight_fn, low, pivot - 1, end);
            }
        }
    }

    assert!(
        total_weight >= W::default(),
        "total weight cannot be negative"
    );

    if slice.is_empty() {
        return 0..0;
    }

    let mut end = 0;
    recurse_fit_maximal_range(
        slice,
        total_weight,
        &mut weight_fn,
        0,
        slice.len() - 1, // won't underflow due to empty check
        &mut end,
    );

    0..end
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn fit_empty_slice() {
        let r = fit_longest_range(&mut [], 100, |i| *i);
        assert!(r.is_empty());
    }

    #[test]
    #[should_panic(expected = "total weight cannot be negative")]
    fn fit_negative_panic() {
        fit_longest_range(&mut [0], -1, |i| *i);
    }

    #[test]
    fn no_fit() {
        let r = fit_longest_range(&mut [100, 101, 102], 99, |i| *i);
        assert!(r.is_empty());
    }

    #[test]
    fn fit_all() {
        let r = fit_longest_range(&mut [1, 2, 3, 4, 5], 15, |i| *i);
        assert_eq!(r.len(), 5);

        let r = fit_longest_range(&mut [5, 4, 3, 2, 1], 20, |i| *i);
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn fit_some() {
        let s = &mut [8, 2, 2, 3, 2, 1, 2, 4, 1];
        let r = fit_longest_range(s, 10, |i| *i);
        assert_eq!(r.len(), 6);
        assert_eq!(s[r.start..r.end].iter().copied().sum::<i32>(), 10);
        assert!(s[r.end..].contains(&8));
        assert!(s[r.end..].contains(&4));
        assert!(s[r.end..].contains(&3));
    }

    #[test]
    fn unlimited_state() {
        let manager_state = TaskManagerState::<()>::new(u64::MAX, u64::MAX);
        assert!(manager_state.unlimited());
    }
}
