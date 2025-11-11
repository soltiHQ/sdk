use async_trait::async_trait;
use taskvisor::{Event, Subscribe};

use crate::subscriber::view::log_event;

pub struct Journal;

impl Journal {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Subscribe for Journal {
    async fn on_event(&self, event: &Event) {
        log_event(event);
    }
    fn name(&self) -> &'static str {
        "journal"
    }
    fn queue_capacity(&self) -> usize {
        2048
    }
}
