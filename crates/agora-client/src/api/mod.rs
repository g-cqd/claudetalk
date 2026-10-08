//! One async function per verb: `(ctx, params) -> Result<Out, ClientError>`.

pub mod board;
pub mod chat;
pub mod org;
pub mod sched;
pub mod work;
pub mod work_text;
