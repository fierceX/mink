//! Shared session transport-copy store; image capture still belongs to Read.
pub(crate) use crate::runtime::AttachmentStore;
pub(crate) use crate::runtime::restrict_private_dir;

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod tests;
