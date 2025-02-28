use super::{
    authenticate::{handle_client_authenticate, Credential},
    client_dialog::ClientInviteDialog,
    server_dialog::ServerInviteDialog,
    DialogId,
};
use crate::{
    header_pop,
    rsip_ext::extract_uri_from_contact,
    transaction::{
        endpoint::EndpointInnerRef,
        key::{TransactionKey, TransactionRole},
        transaction::{Transaction, TransactionEventSender},
    },
    Result,
};
use rsip::{
    headers::Route,
    prelude::{HeadersExt, ToTypedHeader, UntypedHeader},
    typed::{CSeq, Contact},
    Header, Param, Request, Response, SipMessage, StatusCode,
};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// DialogState is the state of the dialog
#[derive(Clone)]
pub enum DialogState {
    Calling(DialogId),
    Trying(DialogId),
    Early(DialogId, rsip::Response),
    WaitAck(DialogId, rsip::Response),
    Confirmed(DialogId),
    Updated(DialogId, rsip::Request),
    Notify(DialogId, rsip::Request),
    Info(DialogId, rsip::Request),
    Terminated(DialogId, Option<rsip::StatusCode>),
}
#[derive(Clone)]
pub enum Dialog {
    ServerInvite(ServerInviteDialog),
    ClientInvite(ClientInviteDialog),
}

pub struct DialogInner {
    pub role: TransactionRole,
    pub cancel_token: CancellationToken,
    pub id: Mutex<DialogId>,
    pub state: Mutex<DialogState>,

    pub local_seq: AtomicU32,
    pub local_contact: Option<rsip::Uri>,

    pub remote_seq: AtomicU32,
    pub remote_uri: rsip::Uri,

    pub from: String,
    pub to: Mutex<String>,

    pub credential: Option<Credential>,
    pub route_set: Vec<Route>,
    pub(super) endpoint_inner: EndpointInnerRef,
    pub(super) state_sender: DialogStateSender,
    pub(super) tu_sender: TuSenderRef,
    pub(super) initial_request: Request,
}

pub type DialogStateReceiver = UnboundedReceiver<DialogState>;
pub type DialogStateSender = UnboundedSender<DialogState>;

pub(super) type DialogInnerRef = Arc<DialogInner>;
pub(super) type TuSenderRef = Mutex<Option<TransactionEventSender>>;

impl DialogState {
    pub fn is_confirmed(&self) -> bool {
        matches!(self, DialogState::Confirmed(_))
    }
}

impl DialogInner {
    pub fn new(
        role: TransactionRole,
        id: DialogId,
        initial_request: Request,
        endpoint_inner: EndpointInnerRef,
        state_sender: DialogStateSender,
        credential: Option<Credential>,
        local_contact: Option<rsip::Uri>,
    ) -> Result<Self> {
        let cseq = initial_request.cseq_header()?.seq()?;

        let remote_uri = match role {
            TransactionRole::Client => initial_request.uri.clone(),
            TransactionRole::Server => {
                extract_uri_from_contact(initial_request.contact_header()?.value())?
            }
        };

        let from = initial_request.from_header()?.typed()?;
        let mut to = initial_request.to_header()?.typed()?;
        if !to.params.iter().any(|p| matches!(p, Param::Tag(_))) {
            to.params.push(rsip::Param::Tag(id.to_tag.clone().into()));
        }

        let (from, to) = match role {
            TransactionRole::Client => (from.to_string(), to.to_string()),
            TransactionRole::Server => (to.to_string(), from.to_string()),
        };
        let route_set = initial_request
            .headers
            .iter()
            .filter_map(|header| {
                if let Header::RecordRoute(rr) = header {
                    Some(Route::from(rr.value()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        Ok(Self {
            role,
            cancel_token: CancellationToken::new(),
            id: Mutex::new(id.clone()),
            from,
            to: Mutex::new(to),
            local_seq: AtomicU32::new(cseq),
            remote_uri,
            remote_seq: AtomicU32::new(cseq),
            credential,
            route_set,
            endpoint_inner,
            state_sender,
            tu_sender: Mutex::new(None),
            state: Mutex::new(DialogState::Calling(id)),
            initial_request,
            local_contact,
        })
    }

    pub fn is_confirmed(&self) -> bool {
        self.state.lock().unwrap().is_confirmed()
    }
    pub fn get_local_seq(&self) -> u32 {
        self.local_seq.load(Ordering::Relaxed)
    }
    pub fn increment_local_seq(&self) -> u32 {
        self.local_seq.fetch_add(1, Ordering::Relaxed);
        self.local_seq.load(Ordering::Relaxed)
    }

    pub fn increment_remove_seq(&self) -> u32 {
        self.remote_seq.fetch_add(1, Ordering::Relaxed);
        self.remote_seq.load(Ordering::Relaxed)
    }

    pub fn update_remote_tag(&self, tag: &str) -> Result<()> {
        self.id.lock().unwrap().to_tag = tag.to_string();
        let to: rsip::headers::untyped::To = self.to.lock().unwrap().clone().into();
        *self.to.lock().unwrap() = to.typed()?.with_tag(tag.to_string().into()).to_string();
        info!("updating remote tag to: {}", self.to.lock().unwrap());
        Ok(())
    }

    pub(super) fn make_request(
        &self,
        method: rsip::Method,
        cseq: Option<u32>,
        branch: Option<Param>,
        headers: Option<Vec<rsip::Header>>,
        body: Option<Vec<u8>>,
    ) -> Result<rsip::Request> {
        let mut headers = headers.unwrap_or_default();
        let cseq_header = CSeq {
            seq: cseq.unwrap_or(self.increment_remove_seq()),
            method,
        };

        let via = self.endpoint_inner.get_via(branch)?;
        headers.push(via.into());
        headers.push(Header::CallId(
            self.id.lock().unwrap().call_id.clone().into(),
        ));
        headers.push(Header::From(self.from.clone().into()));
        headers.push(Header::To(self.to.lock().unwrap().clone().into()));
        headers.push(Header::CSeq(cseq_header.into()));
        headers.push(Header::UserAgent(
            self.endpoint_inner.user_agent.clone().into(),
        ));

        self.local_contact
            .as_ref()
            .map(|c| headers.push(Contact::from(c.clone()).into()));

        for route in &self.route_set {
            headers.push(Header::Route(route.clone()));
        }
        headers.push(Header::MaxForwards(70.into()));

        body.as_ref().map(|b| {
            headers.push(Header::ContentLength((b.len() as u32).into()));
        });

        let req = rsip::Request {
            method,
            uri: self.remote_uri.clone(),
            headers: headers.into(),
            body: body.unwrap_or_default(),
            version: rsip::Version::V2,
        };
        Ok(req)
    }

    pub(super) fn make_response(
        &self,
        request: &Request,
        status: StatusCode,
        headers: Option<Vec<rsip::Header>>,
        body: Option<Vec<u8>>,
    ) -> rsip::Response {
        let mut resp_headers = rsip::Headers::default();
        self.local_contact
            .as_ref()
            .map(|c| resp_headers.push(Contact::from(c.clone()).into()));

        for header in request.headers.iter() {
            match header {
                Header::RecordRoute(route) => {
                    resp_headers.push(Header::RecordRoute(route.clone()));
                }
                Header::Via(via) => {
                    resp_headers.push(Header::Via(via.clone()));
                }
                Header::From(from) => {
                    resp_headers.push(Header::From(from.clone()));
                }
                Header::To(to) => {
                    let mut to = match to.clone().typed() {
                        Ok(to) => to,
                        Err(e) => {
                            info!("error parsing to header {}", e);
                            continue;
                        }
                    };

                    if status != StatusCode::Trying {
                        if !to.params.iter().any(|p| matches!(p, Param::Tag(_))) {
                            to.params.push(rsip::Param::Tag(
                                self.id.lock().unwrap().to_tag.clone().into(),
                            ));
                        }
                    }
                    resp_headers.push(Header::To(to.into()));
                }
                Header::CSeq(cseq) => {
                    resp_headers.push(Header::CSeq(cseq.clone()));
                }
                Header::CallId(call_id) => {
                    resp_headers.push(Header::CallId(call_id.clone()));
                }
                _ => {}
            }
        }

        if let Some(headers) = headers {
            for header in headers {
                resp_headers.unique_push(header);
            }
        }

        body.as_ref().map(|b| {
            resp_headers.push(Header::ContentLength((b.len() as u32).into()));
        });

        resp_headers.unique_push(Header::UserAgent(
            self.endpoint_inner.user_agent.clone().into(),
        ));

        Response {
            status_code: status,
            headers: resp_headers,
            body: body.unwrap_or_default(),
            version: request.version().clone(),
        }
    }

    pub(super) async fn do_request(&self, mut request: Request) -> Result<Option<rsip::Response>> {
        let method = request.method().to_owned();
        let destination = request
            .route_header()
            .map(|r| r.value().try_into().ok())
            .flatten();
        header_pop!(request.headers, Header::Route);

        let key = TransactionKey::from_request(&request, TransactionRole::Client)?;
        let mut tx = Transaction::new_client(key, request, self.endpoint_inner.clone(), None);
        tx.destination = destination;

        tx.send().await?;
        let mut auth_sent = false;

        while let Some(msg) = tx.receive().await {
            match msg {
                SipMessage::Response(resp) => match resp.status_code {
                    StatusCode::Trying => {
                        continue;
                    }
                    StatusCode::Ringing | StatusCode::SessionProgress => {
                        self.transition(DialogState::Early(self.id.lock().unwrap().clone(), resp))?;
                        continue;
                    }
                    StatusCode::ProxyAuthenticationRequired | StatusCode::Unauthorized => {
                        let id = self.id.lock().unwrap().clone();
                        if auth_sent {
                            info!("received {} response after auth sent", resp.status_code);
                            self.transition(DialogState::Terminated(id, Some(resp.status_code)))?;
                            break;
                        }
                        auth_sent = true;
                        if let Some(cred) = &self.credential {
                            let new_seq = match method {
                                rsip::Method::Cancel => self.get_local_seq(),
                                _ => self.increment_local_seq(),
                            };
                            tx = handle_client_authenticate(new_seq, tx, resp, cred).await?;
                            tx.send().await?;
                            continue;
                        } else {
                            info!("received 407 response without auth option");
                            self.transition(DialogState::Terminated(id, Some(resp.status_code)))?;
                        }
                    }
                    _ => {
                        debug!("dialog do_request done: {:?}", resp.status_code);
                        return Ok(Some(resp));
                    }
                },
                _ => break,
            }
        }
        Ok(None)
    }

    pub(super) fn transition(&self, state: DialogState) -> Result<()> {
        self.state_sender.send(state.clone())?;
        match state {
            DialogState::Updated(_, _) | DialogState::Notify(_, _) | DialogState::Info(_, _) => {
                return Ok(());
            }
            _ => {}
        }
        let mut old_state = self.state.lock().unwrap();
        info!("transitioning state: {} -> {}", old_state, state);
        *old_state = state;
        Ok(())
    }
}

impl std::fmt::Display for DialogState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialogState::Calling(id) => write!(f, "{}(Calling)", id),
            DialogState::Trying(id) => write!(f, "{}(Trying)", id),
            DialogState::Early(id, _) => write!(f, "{}(Early)", id),
            DialogState::WaitAck(id, _) => write!(f, "{}(WaitAck)", id),
            DialogState::Confirmed(id) => write!(f, "{}(Confirmed)", id),
            DialogState::Updated(id, _) => write!(f, "{}(Updated)", id),
            DialogState::Notify(id, _) => write!(f, "{}(Notify)", id),
            DialogState::Info(id, _) => write!(f, "{}(Info)", id),
            DialogState::Terminated(id, code) => write!(f, "{}(Terminated {:?})", id, code),
        }
    }
}

impl Dialog {
    pub fn id(&self) -> DialogId {
        match self {
            Dialog::ServerInvite(d) => d.inner.id.lock().unwrap().clone(),
            Dialog::ClientInvite(d) => d.inner.id.lock().unwrap().clone(),
        }
    }
    pub async fn handle(&mut self, tx: Transaction) -> Result<()> {
        match self {
            Dialog::ServerInvite(d) => d.handle(tx).await,
            Dialog::ClientInvite(d) => d.handle(tx).await,
        }
    }
    pub fn on_remove(&self) {
        match self {
            Dialog::ServerInvite(d) => {
                d.inner.cancel_token.cancel();
            }
            Dialog::ClientInvite(d) => {
                d.inner.cancel_token.cancel();
            }
        }
    }
}
