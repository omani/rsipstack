use super::transaction::{TransactionInnerRef, TransactionTimer};
use crate::Result;
#[derive(Clone)]
pub(crate) struct ClientNonInviteHandler {
    pub inner: TransactionInnerRef,
}
impl ClientNonInviteHandler {
    pub(super) async fn on_timer(&self, timer: &TransactionTimer) -> Result<()> {
        Ok(())
    }
}
