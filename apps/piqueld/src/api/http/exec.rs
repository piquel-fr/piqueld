//! Upgraded connections streaming one-off commands. See `piqueld_core::exec`.
use super::{ApiError, ApiPath, ApiState, decode_json, openapi::ApiErrorResponse};
use crate::{api::ExecSession, auth::Identity, docker::ExecIo};
use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{FromRequestParts, State},
    http::{StatusCode, header, request::Parts},
    response::Response,
};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use piqueld_core::{
    ApplicationId,
    exec::{EXEC_PROTOCOL, ExecFrame, ExecInput, ExecOutput, ExecRequest},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
use tower_http::request_id::RequestId;

/// A pending switch to the exec frame protocol, requested by the client.
pub(super) struct ExecUpgrade(OnUpgrade);

impl<S: Send + Sync> FromRequestParts<S> for ExecUpgrade {
    type Rejection = ApiError;

    /// Takes hyper's pending upgrade when the request asks for it with
    /// `Connection: upgrade` and `Upgrade: piqueld-exec.v1` (comma-separated
    /// tokens, case-insensitive).
    ///
    /// # Errors
    /// Rejects with 426 `upgrade_required` when either header is missing or the
    /// connection offers no upgrade.
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let header_contains = |name: header::HeaderName, value: &str| {
            parts
                .headers
                .get_all(name)
                .iter()
                .filter_map(|header| header.to_str().ok())
                .flat_map(|header| header.split(','))
                .any(|token| token.trim().eq_ignore_ascii_case(value))
        };
        let requested = header_contains(header::CONNECTION, "upgrade")
            && header_contains(header::UPGRADE, EXEC_PROTOCOL);
        requested
            .then(|| parts.extensions.remove::<OnUpgrade>())
            .flatten()
            .map(Self)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UPGRADE_REQUIRED,
                    "upgrade_required",
                    "Request an HTTP/1.1 upgrade to the piqueld-exec.v1 protocol",
                )
            })
    }
}

/// Runs a one-off command in a running task of an application service.
///
/// The command inherits the task's image, environment, secrets, mounts and
/// networks; healthy tasks are preferred over tasks still starting. The request
/// must carry `Connection: Upgrade` and `Upgrade: piqueld-exec.v1`, otherwise it
/// fails with 426 `upgrade_required`. Validation, 404 `not_found`, 409
/// `service_not_running` and Docker errors are ordinary JSON errors returned
/// before the upgrade.
///
/// After `101 Switching Protocols`, both directions exchange frames made of a
/// one-byte tag, a big-endian `u32` payload length (at most 1 MiB) and the
/// payload:
///
/// ```text
/// client -> daemon  1 stdin bytes
///                   2 end of stdin (ignored with a terminal; disconnect to detach)
///                   3 terminal resize: u16 width, u16 height
/// daemon -> client  1 stdout bytes (terminal output with tty)
///                   2 stderr bytes
///                   3 final: exit code as big-endian i64
///                   4 final: JSON error body
/// ```
///
/// History records `command_started` and `command_finished` events with the
/// account, task and exit code, never the command.
#[utoipa::path(post,path="/api/v1/applications/{id}/exec",operation_id="execApplicationCommand",params(("id"=String,Path)),request_body=ExecRequest,
 responses((status=101,description="Switched to the piqueld-exec.v1 frame protocol described in this operation's description"),
 (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),
 (status=426,response=inline(ApiErrorResponse)),(status=502,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
// Creates the command and records `command_started` before answering 101, then
// streams on a spawned task once hyper completes the upgrade.
pub(super) async fn exec(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ExecUpgrade(upgrade): ExecUpgrade,
    identity: Option<Extension<Identity>>,
    request_id: Option<Extension<RequestId>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = ApplicationId::parse(id)?;
    let request: ExecRequest = decode_json(&body)?;
    let account = identity.map_or_else(
        || "An unidentified caller".to_owned(),
        |Extension(identity)| identity.user.username,
    );
    let session = state.exec(&id, &request, &account).await?;
    let request_id =
        request_id.and_then(|Extension(id)| id.header_value().to_str().ok().map(str::to_owned));
    tokio::spawn(async move {
        match upgrade.await {
            Ok(connection) => stream(session, TokioIo::new(connection), request_id).await,
            Err(error) => tracing::warn!(?error, "exec connection upgrade failed"),
        }
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, EXEC_PROTOCOL)
        .body(Body::empty())
        .expect("static upgrade response is valid"))
}

/// Relays frames between the client connection and the running command, then
/// sends the final exit or failure frame. Malformed client input ends input
/// forwarding, which closes the command's standard input. When writing to the
/// client fails, the command stream stops and no final frame is written.
async fn stream(
    session: ExecSession,
    connection: impl AsyncRead + AsyncWrite + Send + 'static,
    request_id: Option<String>,
) {
    let (mut reader, mut writer) = tokio::io::split(connection);
    let (input, input_rx) = mpsc::channel(16);
    let (output_tx, output) = mpsc::channel::<ExecOutput>(16);
    // Input is read independently so a quiet client never blocks output.
    // Ending the read drops `input`, which closes the command's standard input.
    let reading = tokio::spawn(async move {
        let mut buffer = Vec::new();
        loop {
            match ExecInput::decode(&mut buffer) {
                Ok(Some(frame)) => {
                    if input.send(frame).await.is_err() {
                        return;
                    }
                }
                Ok(None) => match reader.read_buf(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                },
                Err(error) => {
                    tracing::warn!(%error, "rejected exec input");
                    return;
                }
            }
        }
    });
    // Owning `output` here drops it if the client disconnects, which stops the command stream.
    let writing = async {
        let mut output = output;
        while let Some(frame) = output.recv().await {
            writer.write_all(&frame.encoded()).await?;
        }
        std::io::Result::Ok(())
    };
    let io = ExecIo {
        input: input_rx,
        output: output_tx,
    };
    let (result, written) = tokio::join!(session.run(io), writing);
    reading.abort();
    if written.is_err() {
        return;
    }
    let last = match result {
        Ok(code) => ExecOutput::Exit(code),
        Err(error) => {
            tracing::error!(?error, "exec stream failed");
            let mut body = ApiError::from(error).body();
            if let Some(request_id) = request_id {
                body.request_id = request_id;
            }
            ExecOutput::Failed(body)
        }
    };
    if writer.write_all(&last.encoded()).await.is_ok() {
        let _ = writer.shutdown().await;
    }
}
