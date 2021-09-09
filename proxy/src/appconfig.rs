use std::sync::{Arc, Mutex};

use crate::jaeger_tracing::Tracer;
use crate::tracker::CursorTraceMapper;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub tracer: Option<Tracer>,
    pub trace_mapper: Arc<Mutex<CursorTraceMapper>>,
    pub log_mongo_messages: bool,
    pub use_tls: bool,
    pub skip_host_verification: bool,
    pub service_name: String,
}

impl AppConfig {
    pub fn new(
        tracer: Option<Tracer>,
        log_mongo_messages: bool,
        use_tls: bool,
        skip_host_verification: bool,
        service_name: String,
    ) -> Self {
        AppConfig {
            tracer,
            trace_mapper: Arc::new(Mutex::new(CursorTraceMapper::new())),
            log_mongo_messages,
            use_tls,
            skip_host_verification,
            service_name,
        }
    }
}
