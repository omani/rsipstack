use std::sync::atomic::Ordering;

use super::dialog::DialogInnerRef;
use super::DialogId;
use crate::dialog::{authenticate::handle_client_authenticate, dialog::DialogState};
use crate::transaction::transaction::Transaction;
use crate::Result;
use rsip::prelude::HeadersExt;
use rsip::{Response, SipMessage, StatusCode};
use tokio_util::sync::CancellationToken;
use tracing::{info, info_span, trace};

#[derive(Clone)]
pub struct ClientInviteDialog {
    pub(super) inner: DialogInnerRef,
}

impl ClientInviteDialog {
    pub fn id(&self) -> DialogId {
        self.inner.id.clone()
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.inner.cancel_token
    }

    pub async fn bye(&self) -> Result<()> {
        if !self.inner.is_confirmed() {
            return Ok(());
        }
        let request = self.inner.make_request(rsip::Method::Bye, None, None)?;
        let resp = self.inner.do_request(&request).await?;
        self.inner.transition(DialogState::Terminated(
            self.id(),
            resp.map(|r| r.status_code),
        ))?;
        Ok(())
    }

    pub async fn cancel(&self) -> Result<()> {
        let mut cancel_request = self.inner.initial_request.clone();
        cancel_request.method = rsip::Method::Cancel;
        cancel_request
            .cseq_header_mut()?
            .mut_seq(self.inner.get_local_seq())?;
        cancel_request.body = vec![];
        self.inner.do_request(&cancel_request).await?;
        Ok(())
    }

    pub async fn reinvite(&self) -> Result<()> {
        if !self.inner.is_confirmed() {
            return Ok(());
        }
        todo!()
    }

    pub async fn info(&self) -> Result<()> {
        if !self.inner.is_confirmed() {
            return Ok(());
        }

        let request = self.inner.make_request(rsip::Method::Info, None, None)?;
        self.inner.do_request(&request).await?;
        self.inner
            .transition(DialogState::Info(self.id(), request))?;
        Ok(())
    }

    pub async fn handle(&mut self, mut tx: Transaction) -> Result<()> {
        let span = info_span!("client_invite_dialog", dialog_id = %self.id());
        let _enter = span.enter();

        trace!(
            "handle request: {:?} state:{}",
            tx.original,
            self.inner.state.lock().unwrap()
        );

        let cseq = tx.original.cseq_header()?.seq()?;
        if cseq < self.inner.remote_seq.load(Ordering::Relaxed) {
            info!(
                "received old request remote_seq: {} > {}",
                self.inner.remote_seq.load(Ordering::Relaxed),
                cseq
            );
            tx.reply(rsip::StatusCode::ServerInternalError).await?;
            return Ok(());
        }

        self.inner.remote_seq.store(cseq, Ordering::Relaxed);

        if self.inner.is_confirmed() {
            match tx.original.method {
                rsip::Method::Invite => {}
                rsip::Method::Bye => return self.handle_bye(tx).await,
                rsip::Method::Info => return self.handle_info(tx).await,
                _ => {
                    info!("invalid request method: {:?}", tx.original.method);
                    tx.reply(rsip::StatusCode::MethodNotAllowed).await?;
                    return Err(crate::Error::DialogError(
                        "invalid request".to_string(),
                        self.id(),
                    ));
                }
            }
        } else {
            info!(
                "received request before confirmed: {:?}",
                tx.original.method
            );
        }
        Ok(())
    }

    async fn handle_bye(&mut self, mut tx: Transaction) -> Result<()> {
        info!("received bye");
        self.inner
            .transition(DialogState::Terminated(self.id(), None))?;
        tx.reply(rsip::StatusCode::OK).await?;
        Ok(())
    }

    async fn handle_info(&mut self, mut tx: Transaction) -> Result<()> {
        self.inner
            .transition(DialogState::Info(self.id(), tx.original.clone()))?;
        tx.reply(rsip::StatusCode::OK).await?;
        Ok(())
    }

    pub(super) async fn process_invite(
        &self,
        mut tx: Transaction,
    ) -> Result<(DialogId, Option<Response>)> {
        let span = info_span!("client_dialog", dialog_id = %self.id());
        let _enter = span.enter();

        self.inner.transition(DialogState::Calling(self.id()))?;
        let mut auth_sent = false;
        tx.send().await?;
        let mut dialog_id = self.id();
        let mut final_response = None;
        while let Some(msg) = tx.receive().await {
            match msg {
                SipMessage::Request(_) => {}
                SipMessage::Response(resp) => match resp.status_code {
                    StatusCode::Trying => {
                        self.inner.transition(DialogState::Trying(self.id()))?;
                    }
                    StatusCode::Ringing | StatusCode::SessionProgress => {
                        self.inner.transition(DialogState::Early(self.id(), resp))?;
                    }
                    StatusCode::OK => {
                        let ack = rsip::Request {
                            method: rsip::Method::Ack,
                            uri: tx.original.uri.clone(),
                            headers: resp.headers.clone(),
                            version: rsip::Version::V2,
                            body: Default::default(),
                        };
                        dialog_id = DialogId::try_from(&ack)?.clone();
                        final_response = Some(resp.clone());
                        tx.send_ack(ack).await?;
                        self.inner
                            .transition(DialogState::Confirmed(dialog_id.clone(), resp))?;
                        break;
                    }
                    StatusCode::Decline | StatusCode::RequestTerminated => {
                        info!("received terminated response: {}", resp.status_code);
                        self.inner.transition(DialogState::Terminated(
                            self.id(),
                            Some(resp.status_code),
                        ))?;
                    }
                    StatusCode::ProxyAuthenticationRequired | StatusCode::Unauthorized => {
                        if auth_sent {
                            info!("received {} response after auth sent", resp.status_code);
                            self.inner.transition(DialogState::Terminated(
                                self.id(),
                                Some(resp.status_code),
                            ))?;
                            break;
                        }
                        auth_sent = true;
                        if let Some(credential) = &self.inner.credential {
                            tx = handle_client_authenticate(
                                self.inner.increment_local_seq(),
                                tx,
                                resp,
                                credential,
                            )
                            .await?;
                            tx.send().await?;
                            continue;
                        } else {
                            info!("received 407 response without auth option");
                            self.inner.transition(DialogState::Terminated(
                                self.id(),
                                Some(resp.status_code),
                            ))?;
                        }
                    }
                    _ => match resp.status_code.kind() {
                        rsip::StatusCodeKind::Redirection => {
                            self.inner.transition(DialogState::Terminated(
                                self.id(),
                                Some(resp.status_code),
                            ))?;
                        }
                        rsip::StatusCodeKind::RequestFailure
                        | rsip::StatusCodeKind::ServerFailure
                        | rsip::StatusCodeKind::GlobalFailure => {
                            info!("received failure response: {}", resp.status_code);
                            self.inner.transition(DialogState::Terminated(
                                self.id(),
                                Some(resp.status_code),
                            ))?;
                        }
                        _ => {
                            info!("ignoring response: {}", resp.status_code);
                        }
                    },
                },
            }
        }
        trace!("process done");
        Ok((dialog_id, final_response))
    }
}
