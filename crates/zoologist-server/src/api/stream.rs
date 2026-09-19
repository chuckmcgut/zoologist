//! `GET /stream`: live event updates as Server-Sent Events.
//!
//! Each message is `event: started|updated|ended`, `id: <event id>`, `data: <Event JSON>`.
//! A client reconnecting with `Last-Event-ID` first receives every newer event from the
//! database, then live updates.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::broadcast;
use zoologist_store::{EventQuery, EventRecord, MAX_PAGE, Order};

use super::{ApiResult, EventJson};
use crate::app::{ApiEvent, AppState};

const KEEP_ALIVE: Duration = Duration::from_secs(15);

fn sse(kind: &str, record: &EventRecord) -> Event {
    let data = serde_json::to_string(&EventJson::new(record)).unwrap_or_default();
    Event::default()
        .event(kind)
        .id(record.id.to_string())
        .data(data)
}

fn live_event(msg: &ApiEvent) -> Event {
    let kind = match msg {
        ApiEvent::Started(_) => "started",
        ApiEvent::Updated(_) => "updated",
        ApiEvent::Ended(_) => "ended",
    };
    sse(kind, msg.record())
}

pub async fn stream(
    State(app): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    // Subscribe before reading the backlog so nothing falls between the two.
    let rx = app.events.subscribe();
    let last_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let backlog = match last_id {
        Some(after) => {
            let query = EventQuery {
                after_id: Some(after),
                limit: MAX_PAGE,
                order: Order::Asc,
                ..Default::default()
            };
            app.store.call(move |s| s.list_events(&query)).await?.items
        }
        None => Vec::new(),
    };
    let replay = stream::iter(backlog.into_iter().map(|r| {
        let kind = if r.ended_at.is_some() {
            "ended"
        } else {
            "started"
        };
        Ok(sse(kind, &r))
    }));
    let live = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(msg) => return Some((Ok(live_event(&msg)), rx)),
                // A slow client missed some updates; the next ones still arrive.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!("SSE client lagged by {n} messages");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let shutdown = app.shutdown.clone();
    let events = replay
        .chain(live)
        .take_until(async move { shutdown.cancelled().await });
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}
