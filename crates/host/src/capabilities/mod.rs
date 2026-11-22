pub mod discovery;
pub mod files;
pub mod net;
pub mod process;

use crate::capability::CapabilityCatalog;

pub fn register_builtin_capabilities(catalog: &CapabilityCatalog) {
    files::register(catalog);
    process::register(catalog);
    net::register(catalog);
    crate::jobs::register_job_capabilities(catalog);
    crate::comms::register_comms_capabilities(catalog);
    discovery::register(catalog);
}
