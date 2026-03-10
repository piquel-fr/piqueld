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

#[macro_export]
macro_rules! service {
    (
        $service:ident, $impl_type:ident, $error:ty;
        $( $method:ident ( $( $param:ident : $param_type:ty ),* ) -> $ret:ty ),* $(,)?
    ) => {
        ::paste::paste! {

            enum [<$service Command>] {
                $(
                    [<$method:camel>] {
                        $( $param: $param_type, )*
                        reply: ::tokio::sync::oneshot::Sender<
                            ::std::result::Result<$ret, $error>
                        >,
                    },
                )*
            }

            pub struct $service {
                tx: ::tokio::sync::mpsc::Sender<[<$service Command>]>,
            }

            impl $service {
                pub fn init(
                    config: &crate::config::ServerConfig,
                ) -> ::std::result::Result<Self, $error> {
                    let (tx, mut rx) =
                        ::tokio::sync::mpsc::channel::<[<$service Command>]>(32);
                    let mut service = $impl_type::init(config)?;

                    ::tokio::spawn(async move {
                        while let Some(cmd) = rx.recv().await {
                            match cmd {
                                $(
                                    [<$service Command>]::[<$method:camel>] {
                                        $( $param, )* reply
                                    } => {
                                        let _ = reply.send(service.$method( $( $param, )* ));
                                    }
                                )*
                            }
                        }
                    });

                    Ok(Self { tx })
                }

                $(
                    pub async fn $method(
                        &self,
                        $( $param: $param_type, )*
                    ) -> ::std::result::Result<$ret, $error> {
                        let (reply, rx) = ::tokio::sync::oneshot::channel();
                        crate::services::ask(
                            &self.tx,
                            [<$service Command>]::[<$method:camel>] {
                                $( $param, )* reply
                            },
                            rx,
                        ).await
                    }
                )*
            }
        }
    };
}
