// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};

use data_encoding::BASE64URL_NOPAD;
use headers::HeaderValue;
use hyper::{
    header::{CONTENT_TYPE, LOCATION},
    Body, StatusCode,
};
use itertools::Itertools;
use oauth2_types::{
    pkce,
    requests::{
        AccessTokenResponse, AuthorizationRequest, AuthorizationResponse, ResponseMode,
        ResponseType,
    },
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use url::Url;
use warp::{
    reply::{with_header, Response},
    Filter, Rejection, Reply,
};

use crate::{
    config::{CookiesConfig, OAuth2ClientConfig, OAuth2Config},
    errors::WrapError,
    filters::{session::with_optional_session, with_pool, with_templates},
    storage::{oauth2::start_session, SessionInfo},
    templates::{FormPostContext, Templates},
};

fn back_to_client<T>(
    mut redirect_uri: Url,
    response_mode: ResponseMode,
    params: T,
    templates: &Templates,
) -> anyhow::Result<Box<dyn Reply>>
where
    T: Serialize,
{
    #[derive(Serialize)]
    struct AllParams<'s, T> {
        #[serde(flatten, skip_serializing_if = "Option::is_none")]
        existing: Option<HashMap<&'s str, &'s str>>,

        #[serde(flatten)]
        params: T,
    }

    match response_mode {
        ResponseMode::Query => {
            let existing: Option<HashMap<&str, &str>> = redirect_uri
                .query()
                .map(|qs| serde_urlencoded::from_str(qs))
                .transpose()?;

            let merged = AllParams { existing, params };

            let new_qs = serde_urlencoded::to_string(merged)?;

            redirect_uri.set_query(Some(&new_qs));
        }
        ResponseMode::Fragment => {
            let existing: Option<HashMap<&str, &str>> = redirect_uri
                .fragment()
                .map(|qs| serde_urlencoded::from_str(qs))
                .transpose()?;

            let merged = AllParams { existing, params };

            let new_qs = serde_urlencoded::to_string(merged)?;

            redirect_uri.set_fragment(Some(&new_qs));
        }
        ResponseMode::FormPost => {
            let ctx = FormPostContext::new(redirect_uri, params);
            let rendered = templates.render_form_post(&ctx)?;
            return Ok(Box::new(with_header(rendered, CONTENT_TYPE, "text/html")));
        }
    };

    Ok(Box::new(with_header(
        StatusCode::SEE_OTHER,
        LOCATION,
        HeaderValue::from_str(redirect_uri.as_str())?,
    )))
}

#[derive(Deserialize)]
struct Params {
    #[serde(flatten)]
    auth: AuthorizationRequest,

    #[serde(flatten)]
    pkce: Option<pkce::Request>,
}

/// Given a list of response types and an optional user-defined response mode,
/// figure out what response mode must be used, and emit an error if the
/// suggested response mode isn't allowed for the given response types.
fn resolve_response_mode(
    response_type: &HashSet<ResponseType>,
    suggested_response_mode: Option<ResponseMode>,
) -> anyhow::Result<ResponseMode> {
    use ResponseMode as M;
    use ResponseType as T;

    // If the response type includes either "token" or "id_token", the default
    // response mode is "fragment" and the response mode "query" must not be
    // used
    if response_type.contains(&T::Token) || response_type.contains(&T::IdToken) {
        match suggested_response_mode {
            None => Ok(M::Fragment),
            Some(M::Query) => Err(anyhow::anyhow!("invalid response mode")),
            Some(mode) => Ok(mode),
        }
    } else {
        // In other cases, all response modes are allowed, defaulting to "query"
        Ok(suggested_response_mode.unwrap_or(M::Query))
    }
}

pub fn filter(
    pool: &PgPool,
    templates: &Templates,
    oauth2_config: &OAuth2Config,
    cookies_config: &CookiesConfig,
) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone + Send + Sync + 'static {
    let clients = oauth2_config.clients.clone();
    warp::get()
        .and(warp::path!("oauth2" / "authorize"))
        .map(move || clients.clone())
        .and(warp::query())
        .and(with_optional_session(pool, cookies_config))
        .and(with_pool(pool))
        .and(with_templates(templates))
        .and_then(get)
}

async fn get(
    clients: Vec<OAuth2ClientConfig>,
    params: Params,
    maybe_session: Option<SessionInfo>,
    pool: PgPool,
    templates: Templates,
) -> Result<Box<dyn Reply>, Rejection> {
    // First, find out what client it is
    let client = clients
        .into_iter()
        .find(|client| client.client_id == params.auth.client_id)
        .ok_or_else(|| anyhow::anyhow!("could not find client"))
        .wrap_error()?;

    // Then, figure out the redirect URI
    let redirect_uri = client
        .resolve_redirect_uri(&params.auth.redirect_uri)
        .wrap_error()?;

    // Start a DB transaction
    let mut txn = pool.begin().await.wrap_error()?;
    let maybe_session_id = maybe_session.as_ref().map(SessionInfo::key);

    let scope: String = {
        let it = params.auth.scope.iter().map(ToString::to_string);
        Itertools::intersperse(it, " ".to_string()).collect()
    };

    let response_type = &params.auth.response_type;
    let response_mode =
        resolve_response_mode(response_type, params.auth.response_mode).wrap_error()?;

    let oauth2_session = start_session(
        &mut txn,
        maybe_session_id,
        &client.client_id,
        &scope,
        params.auth.state.as_deref(),
        params.auth.nonce.as_deref(),
        params.auth.max_age,
        response_type,
        response_mode,
    )
    .await
    .wrap_error()?;

    let code = if response_type.contains(&ResponseType::Code) {
        // 192bit random bytes encoded in base64, which gives a 32 character code
        let code: [u8; 24] = rand::random();
        let code = BASE64URL_NOPAD.encode(&code);
        Some(
            oauth2_session
                .add_code(&mut txn, &code, &params.pkce)
                .await
                .wrap_error()?,
        )
    } else {
        None
    };

    // Do we have a user in this session, with a last authentication time that
    // matches the requirement?
    let user_session = oauth2_session.fetch_session(&mut txn).await.wrap_error()?;
    if let Some(user_session) = user_session {
        if user_session.active && user_session.last_authd_at >= oauth2_session.max_auth_time() {
            // Yep! Let's complete the auth now
            let mut params = AuthorizationResponse {
                state: oauth2_session.state.clone(),
                ..AuthorizationResponse::default()
            };

            // Did they request an auth code?
            if let Some(ref code) = code {
                params.code = Some(code.code.clone());
            }

            // Did they request an access token?
            if response_type.contains(&ResponseType::Token) {
                // TODO: generate and store an access token
                params.access_token = Some(AccessTokenResponse::new(
                    "some_static_token_that_should_be_generated".into(),
                ));
            }

            // Did they request an ID token?
            if response_type.contains(&ResponseType::IdToken) {
                todo!("id tokens are not implemented yet");
            }

            txn.commit().await.wrap_error()?;
            let reply = back_to_client(redirect_uri.clone(), response_mode, params, &templates)
                .wrap_error()?;
            return Ok(reply);
        }
        // TODO: show reauth form
    }

    // TODO: show login form

    txn.commit().await.wrap_error()?;
    Ok(Box::new(warp::reply::json(&serde_json::json!({
        "session": oauth2_session,
        "code": code,
        "redirect_uri": redirect_uri,
    }))))
}
