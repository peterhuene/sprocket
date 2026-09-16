//! An implementation of OAuth device authorization used by task execution
//! backends.

use anyhow::Context;
use anyhow::Result;
use oauth2::AccessToken;
use oauth2::AuthUrl;
use oauth2::ClientId;
use oauth2::ClientSecret;
use oauth2::DeviceAuthorizationUrl;
use oauth2::RefreshToken;
use oauth2::Scope;
use oauth2::StandardDeviceAuthorizationResponse;
use oauth2::TokenResponse;
use oauth2::TokenUrl;
use oauth2::basic::BasicClient;
use oauth2::reqwest;
use secrecy::ExposeSecret;

use crate::config::OAuthConfig;

/// Performs a OAuth device authorization flow given the OAuth configuration.
///
/// Upon success, returns the OAuth access token and optional refresh token.
pub async fn perform_oauth_device_flow(
    config: &OAuthConfig,
) -> Result<(AccessToken, Option<RefreshToken>)> {
    let mut client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_auth_uri(AuthUrl::new(config.authorization.to_string())?)
        .set_token_uri(TokenUrl::new(config.token.to_string()).context("invalid OAuth token URI")?)
        .set_device_authorization_url(
            DeviceAuthorizationUrl::new(config.authorization.to_string())
                .context("invalid OAuth authorization URI")?,
        );

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build HTTP client for OAuth device authorization flow")?;

    let auth_response: StandardDeviceAuthorizationResponse = client
        .exchange_device_code()
        .add_scopes(config.scopes.iter().cloned().map(Scope::new))
        .request_async(&http_client)
        .await
        .context("failed to request OAuth device authorization")?;

    println!(
        "authorization is required: open {uri} in your web browser and enter code `{code}`",
        uri = auth_response.verification_uri(),
        code = auth_response.user_code().secret()
    );

    if let Some(secret) = &config.client_secret {
        client = client.set_client_secret(ClientSecret::new(
            secret.inner().expose_secret().to_string(),
        ));
    }

    let response = client
        .exchange_device_access_token(&auth_response)
        .request_async(&http_client, tokio::time::sleep, None)
        .await
        .context("failed to exchange OAuth device access token")?;

    dbg!(&response);
    Ok((
        response.access_token().clone(),
        response.refresh_token().cloned(),
    ))
}
