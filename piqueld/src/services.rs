pub mod git;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("actor send channel closed unexpectedly")]
    Send,
    #[error("actor recv channel closed unexpectedly")]
    Recv,
}

pub async fn ask<C, T, E>(
    tx: &mpsc::Sender<C>,
    cmd: C,
    rx: oneshot::Receiver<Result<T, E>>,
) -> Result<T, E>
where
    E: From<ChannelError>,
{
    tx.send(cmd).await.map_err(|_| ChannelError::Send.into())?;
    rx.await.map_err(|_| ChannelError::Recv.into())?
}
