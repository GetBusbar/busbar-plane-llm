// SPDX-License-Identifier: Apache-2.0
//! Codec diagnostic events observed while a concrete codec operation runs on this thread.
#![allow(dead_code)]
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{layer::Context, prelude::*, Layer};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodecDiagnostic {
    pub message: String,
    pub path: Option<String>,
    pub diag: Option<String>,
    pub media_kind: Option<String>,
    // Keep field order and values: existing per-event equalities include more than these labels.
    pub fields: Vec<(String, String)>,
}
impl CodecDiagnostic {
    pub fn text(&self) -> String {
        let fields = self
            .fields
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{} {}", self.message, fields)
    }
    fn field(&mut self, field: &Field, value: String) {
        match field.name() {
            "message" => {
                self.message = value;
                return;
            }
            "path" => self.path = Some(value.clone()),
            "diag" => self.diag = Some(value.clone()),
            "media_kind" => self.media_kind = Some(value.clone()),
            _ => {}
        }
        self.fields.push((field.name().into(), value));
    }
}
impl Visit for CodecDiagnostic {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.field(field, format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.field(field, value.into());
    }
}
pub type DiagnosticEvents = Arc<Mutex<Vec<CodecDiagnostic>>>;
struct CodecEvents {
    events: DiagnosticEvents,
    level: Level,
}
impl<S: Subscriber> Layer<S> for CodecEvents {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if *event.metadata().level() <= self.level {
            let mut diagnostic = CodecDiagnostic::default();
            event.record(&mut diagnostic);
            self.events
                .lock()
                .expect("codec diagnostics")
                .push(diagnostic);
        }
    }
}
pub fn warnings() -> (DiagnosticEvents, tracing::Dispatch) {
    diagnostics(Level::WARN)
}
pub fn debug_diagnostics() -> (DiagnosticEvents, tracing::Dispatch) {
    diagnostics(Level::DEBUG)
}
fn diagnostics(level: Level) -> (DiagnosticEvents, tracing::Dispatch) {
    let events = DiagnosticEvents::default();
    let subscriber = tracing_subscriber::registry().with(CodecEvents {
        events: events.clone(),
        level,
    });
    (events, tracing::Dispatch::new(subscriber))
}
pub fn diagnostic_texts(events: &DiagnosticEvents) -> Vec<String> {
    events
        .lock()
        .expect("codec diagnostics")
        .iter()
        .map(CodecDiagnostic::text)
        .collect()
}
