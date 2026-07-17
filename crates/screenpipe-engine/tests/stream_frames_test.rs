use chrono::{DateTime, Duration, Utc};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

/// Request format for stream frames WebSocket
#[derive(Debug, Serialize)]
struct StreamFramesRequest {
    start_time: String,
    end_time: String,
    order: String,
}

/// Response format from stream frames WebSocket
#[derive(Debug, Deserialize)]
struct StreamTimeSeriesResponse {
    timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use screenpipe_audio::{
        audio_manager::AudioManagerBuilder,
        meeting_streaming::{
            start_meeting_streaming_loop_with_callback, MeetingAudioTap, MeetingStreamingConfig,
            MeetingStreamingProvider, MeetingTranscriptInsertCallback,
        },
        TranscriptionEngine,
    };
    use screenpipe_db::{DatabaseManager, OcrEngine};
    use screenpipe_engine::{
        hot_frame_cache::{HotFrame, HotFrameCache},
        SCServer,
    };
    use std::{
        net::SocketAddr,
        sync::{atomic::AtomicBool, Arc},
    };
    use tokio::sync::{broadcast, RwLock};

    #[derive(Debug, Serialize)]
    struct StreamFramesLimitedRequest {
        start_time: String,
        end_time: String,
        order: String,
        limit: usize,
    }

    async fn setup_stream_test_server() -> (
        String,
        Arc<DatabaseManager>,
        Arc<HotFrameCache>,
        tokio::task::JoinHandle<Result<(), std::io::Error>>,
    ) {
        let unique_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let screenpipe_dir = std::env::temp_dir().join(format!(
            "screenpipe-stream-test-{}-{unique_suffix}",
            std::process::id()
        ));

        let db = Arc::new(
            DatabaseManager::new("sqlite::memory:", Default::default())
                .await
                .unwrap(),
        );
        let audio_manager = Arc::new(
            AudioManagerBuilder::new()
                .is_disabled(true)
                .output_path(screenpipe_dir.join("audio"))
                .build(db.clone())
                .await
                .unwrap(),
        );

        let hot_frame_cache = Arc::new(HotFrameCache::new());
        // Mark the cache warm even when the DB has no visual frames. This keeps
        // audio-only current-day tests from waiting on the production warm-up.
        hot_frame_cache.warm_from_db(&db, 1).await;
        let mut server = SCServer::new(
            db.clone(),
            SocketAddr::from(([127, 0, 0, 1], 0)),
            screenpipe_dir,
            false,
            false,
            audio_manager,
            false,
            "balanced".to_string(),
        );
        server.hot_frame_cache = Some(hot_frame_cache.clone());
        let app = server.create_router().await;
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await });

        (
            format!("ws://{addr}/stream/frames"),
            db,
            hot_frame_cache,
            handle,
        )
    }

    #[tokio::test]
    async fn test_dense_range_stream_spans_full_requested_window() {
        let (url, db, _hot_frame_cache, server_handle) = setup_stream_test_server().await;
        let device_name = "stream-regression-monitor";
        db.insert_video_chunk("stream-regression.mp4", device_name)
            .await
            .unwrap();

        let start = DateTime::parse_from_rfc3339("2026-06-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let total_frames = 3_000usize;
        let display_limit = 2_500usize;
        let seeded_frames: Vec<_> = (0..total_frames)
            .map(|idx| {
                (
                    start + Duration::seconds(idx as i64),
                    idx as i64,
                    Vec::new(),
                )
            })
            .collect();
        db.insert_multi_frames_with_ocr_batch(
            device_name,
            &seeded_frames,
            Arc::new(OcrEngine::Tesseract),
        )
        .await
        .unwrap();

        let end = start + Duration::seconds(total_frames as i64 - 1);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("websocket should connect");
        let (mut write, mut read) = ws_stream.split();
        let request = StreamFramesLimitedRequest {
            start_time: start.to_rfc3339(),
            end_time: end.to_rfc3339(),
            order: "descending".to_string(),
            limit: display_limit,
        };

        write
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("request should send");

        let received_frames = timeout(std::time::Duration::from_secs(10), async {
            let mut received = Vec::with_capacity(display_limit);
            while received.len() < display_limit {
                let msg = read
                    .next()
                    .await
                    .expect("websocket should stay open")
                    .expect("message should read");
                let Message::Text(text) = msg else {
                    continue;
                };
                if text == "\"keep-alive-text\"" {
                    continue;
                }
                let mut batch: Vec<StreamTimeSeriesResponse> =
                    serde_json::from_str(&text).expect("response batch should parse");
                received.append(&mut batch);
            }
            received
        })
        .await
        .expect("stream should return dense range in time");

        let first = DateTime::parse_from_rfc3339(&received_frames.first().unwrap().timestamp)
            .unwrap()
            .with_timezone(&Utc);
        let last = DateTime::parse_from_rfc3339(&received_frames.last().unwrap().timestamp)
            .unwrap()
            .with_timezone(&Utc);

        server_handle.abort();

        assert_eq!(received_frames.len(), display_limit);
        assert_eq!(first, end);
        assert_eq!(last, start);
    }

    /// Reproduces the current-day Timeline gap for live meeting transcription:
    /// the final is durable in `meeting_transcript_segments`, but the open
    /// hot-cache WebSocket must also receive it immediately as `audio_update`,
    /// even when no visual frame exists for the static/audio-only period.
    #[tokio::test]
    async fn test_live_meeting_final_is_streamed_to_today_timeline() {
        let (url, db, hot_frame_cache, server_handle) = setup_stream_test_server().await;
        let now = Utc::now();

        let meeting_id = db
            .insert_meeting("Google Meet", "ui_scan", None, None)
            .await
            .unwrap();
        let (audio_tx, audio_rx) = broadcast::channel(8);
        let audio_tap = MeetingAudioTap::new(audio_tx, Arc::new(AtomicBool::new(false)));
        let callback_cache = hot_frame_cache.clone();
        let runtime = tokio::runtime::Handle::current();
        let on_insert: MeetingTranscriptInsertCallback = Arc::new(move |info| {
            let cache = callback_cache.clone();
            runtime.spawn(async move {
                cache.push_meeting_transcript_insert(info).await;
            });
        });
        let coordinator = start_meeting_streaming_loop_with_callback(
            MeetingStreamingConfig::default().with_provider(MeetingStreamingProvider::Disabled),
            audio_tap,
            audio_rx,
            db.clone(),
            Arc::new(RwLock::new(None::<TranscriptionEngine>)),
            Some(on_insert),
        );

        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("websocket should connect");
        let (mut write, mut read) = ws_stream.split();
        let request = StreamFramesRequest {
            start_time: (now - Duration::minutes(3)).to_rfc3339(),
            end_time: (now + Duration::minutes(1)).to_rfc3339(),
            order: "descending".to_string(),
        };
        write
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("current-day request should send");

        // Deterministic subscription handshake: a frame two minutes away from
        // the transcript is outside the audio association window, but seeing
        // it proves the WebSocket request loop has enabled live delivery.
        let handshake_frame = HotFrame {
            frame_id: 91,
            timestamp: now - Duration::minutes(2),
            device_name: "monitor_0".into(),
            app_name: "Google Chrome".into(),
            window_name: "Google Meet".into(),
            ocr_text_preview: "subscription ready".into(),
            snapshot_path: "/tmp/subscription-ready.jpg".into(),
            browser_url: Some("https://meet.google.com/test".into()),
            capture_trigger: "test".into(),
            offset_index: 0,
            fps: 1.0,
            machine_id: None,
        };
        timeout(std::time::Duration::from_secs(3), async {
            let mut retry = tokio::time::interval(std::time::Duration::from_millis(20));
            loop {
                tokio::select! {
                    _ = retry.tick() => {
                        hot_frame_cache.push_frame(handshake_frame.clone()).await;
                    }
                    message = read.next() => {
                        let Some(message) = message else {
                            panic!("websocket closed before subscription handshake");
                        };
                        let Message::Text(text) = message.expect("subscription handshake should read") else {
                            continue;
                        };
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        if value.as_array().is_some_and(|frames| {
                            frames.iter().any(|frame| {
                                frame
                                    .pointer("/devices/0/frame_id")
                                    .and_then(|value| value.as_i64())
                                    == Some(91)
                            })
                        }) {
                            return;
                        }
                    }
                }
            }
        })
        .await
        .expect("current-day live subscription should become ready");

        let transcript = "the live transcript should appear without reopening timeline";
        screenpipe_events::send_event(
            "meeting_transcript_final",
            serde_json::json!({
                "meeting_id": meeting_id,
                "provider": "selected-engine",
                "model": "test-model",
                "item_id": format!("timeline-e2e-{meeting_id}"),
                "device_name": "System Audio",
                "device_type": "output",
                "speaker_name": "Speaker 1",
                "transcript": transcript,
                "captured_at": now,
            }),
        )
        .expect("live final event should send");

        let (update, audio_only_frame) = timeout(std::time::Duration::from_secs(3), async {
            let mut update = None;
            let mut audio_only_frame = None;
            while let Some(message) = read.next().await {
                let Message::Text(text) = message.expect("live timeline message should read")
                else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if value.get("type").and_then(|v| v.as_str()) == Some("audio_update") {
                    update = Some(value);
                } else if let Some(frames) = value.as_array() {
                    for frame in frames {
                        let has_transcript = frame
                            .get("devices")
                            .and_then(|devices| devices.as_array())
                            .is_some_and(|devices| {
                                devices.iter().any(|device| {
                                    device.get("device_id").and_then(|id| id.as_str())
                                        == Some("audio-only")
                                        && device
                                            .get("audio")
                                            .and_then(|audio| audio.as_array())
                                            .is_some_and(|audio| {
                                                audio.iter().any(|entry| {
                                                    entry
                                                        .get("transcription")
                                                        .and_then(|text| text.as_str())
                                                        == Some(transcript)
                                                })
                                            })
                                })
                            });
                        if has_transcript {
                            audio_only_frame = Some(frame.clone());
                            break;
                        }
                    }
                }

                if update.is_some() && audio_only_frame.is_some() {
                    return (update.unwrap(), audio_only_frame.unwrap());
                }
            }
            panic!("websocket closed before live transcript and audio-only marker");
        })
        .await
        .expect("persisted live final should be visible on the current-day Timeline");

        assert_eq!(
            audio_only_frame
                .get("timestamp")
                .and_then(|value| value.as_str()),
            update.get("timestamp").and_then(|value| value.as_str()),
            "the audio-only marker should stay at the live final's capture time"
        );
        assert_eq!(
            audio_only_frame
                .pointer("/devices/0/audio/0/audio_timestamp")
                .and_then(|value| value.as_str()),
            update.get("timestamp").and_then(|value| value.as_str()),
            "initial/reconnect frame payloads should retain the exact audio time"
        );

        assert_eq!(
            update
                .pointer("/audio/transcription")
                .and_then(|v| v.as_str()),
            Some(transcript)
        );
        assert!(
            update
                .pointer("/audio/audio_chunk_id")
                .and_then(|v| v.as_i64())
                .is_some_and(|id| id < 0),
            "live transcript should use a stable synthetic negative chunk id"
        );
        assert_eq!(
            update
                .pointer("/audio/speaker_name")
                .and_then(|v| v.as_str()),
            Some("Speaker 1"),
            "the live caption should keep the provider speaker label"
        );
        let update_timestamp = update
            .get("timestamp")
            .and_then(|value| value.as_str())
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .expect("audio update should carry its capture timestamp");
        assert_eq!(update_timestamp.timestamp(), now.timestamp());

        // The callback that produced both WebSocket messages only fires after
        // the insert commits, so this read does not race SQLite's writer lock.
        let persisted = db
            .list_meeting_transcript_segments(meeting_id)
            .await
            .expect("persisted live final should be queryable after its update");
        assert!(persisted
            .iter()
            .any(|segment| segment.transcript == transcript));

        // Reconnect after the broadcast has passed. The initial hot-cache
        // payload must still contain the marker and exact audio timestamp; a
        // replayed audio_update will not exist to repair it on the client.
        let (reconnected, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("timeline should reconnect");
        let (mut reconnect_write, mut reconnect_read) = reconnected.split();
        reconnect_write
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("reconnect request should send");
        let replayed_frame = timeout(std::time::Duration::from_secs(3), async {
            while let Some(message) = reconnect_read.next().await {
                let Message::Text(text) = message.expect("reconnect payload should read") else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(frames) = value.as_array() else {
                    continue;
                };
                for frame in frames {
                    if frame
                        .pointer("/devices/0/audio/0/transcription")
                        .and_then(|value| value.as_str())
                        == Some(transcript)
                    {
                        return frame.clone();
                    }
                }
            }
            panic!("reconnect closed before replaying the live final");
        })
        .await
        .expect("reconnect should replay the cached live final");
        assert_eq!(
            replayed_frame
                .pointer("/devices/0/audio/0/audio_timestamp")
                .and_then(|value| value.as_str()),
            update.get("timestamp").and_then(|value| value.as_str())
        );
        assert!(
            db.get_meeting_by_id(meeting_id)
                .await
                .expect("meeting should still exist")
                .meeting_end
                .is_none(),
            "regression must pass before the meeting ends"
        );

        coordinator.abort();
        server_handle.abort();
    }

    /// TEST 1: Reproduce the main issue - new frames after initial fetch are not pushed
    ///
    /// This test verifies the bug where:
    /// 1. Client connects and requests today's frames
    /// 2. Server streams existing frames
    /// 3. NEW frame is inserted into DB
    /// 4. Client does NOT receive the new frame (BUG!)
    #[tokio::test]
    #[ignore = "requires running server, run with: cargo test stream_frames -- --ignored"]
    async fn test_new_frames_not_pushed_to_client_bug() {
        // This test documents the current buggy behavior
        // After fix, this test should be updated to expect the new frame

        let url = "ws://127.0.0.1:3030/stream/frames";
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect to websocket");

        let (mut write, mut read) = ws_stream.split();

        // Request frames for today
        let now = Utc::now();
        let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
        let end_of_day = now.date_naive().and_hms_opt(23, 59, 59).unwrap();

        let request = StreamFramesRequest {
            start_time: format!("{}Z", start_of_day),
            end_time: format!("{}Z", end_of_day),
            order: "descending".to_string(),
        };

        write
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("Failed to send request");

        // Read initial frames (with timeout)
        let mut received_frames = Vec::new();
        let _initial_fetch = timeout(std::time::Duration::from_secs(5), async {
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg {
                    if text == "\"keep-alive-text\"" {
                        break; // End of initial batch
                    }
                    if let Ok(frames) = serde_json::from_str::<Vec<StreamTimeSeriesResponse>>(&text)
                    {
                        received_frames.extend(frames);
                    }
                }
            }
        })
        .await;

        println!("Received {} frames in initial fetch", received_frames.len());

        // Now wait for any new frames (this should timeout with current bug)
        let wait_for_new = timeout(std::time::Duration::from_secs(10), async {
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg {
                    if text != "\"keep-alive-text\"" {
                        println!("Received new frame after initial fetch: {}", text);
                        return true;
                    }
                }
            }
            false
        })
        .await;

        // With current bug, this should timeout (no new frames pushed)
        // After fix, this should receive the new frame
        match wait_for_new {
            Ok(received) => {
                if received {
                    println!("SUCCESS: New frames ARE being pushed (fix is working)");
                } else {
                    println!("BUG CONFIRMED: No new frames received");
                }
            }
            Err(_) => {
                println!("BUG CONFIRMED: Timeout waiting for new frames");
            }
        }
    }

    /// TEST 2: Multiple clients should all receive new frames
    #[tokio::test]
    #[ignore = "requires running server, run with: cargo test stream_frames -- --ignored"]
    async fn test_multiple_clients_receive_new_frames() {
        let url = "ws://127.0.0.1:3030/stream/frames";

        // Connect two clients
        let (ws1, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect client 1");
        let (ws2, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect client 2");

        let (mut write1, mut read1) = ws1.split();
        let (mut write2, mut read2) = ws2.split();

        let now = Utc::now();
        let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
        let end_of_day = now.date_naive().and_hms_opt(23, 59, 59).unwrap();

        let request = StreamFramesRequest {
            start_time: format!("{}Z", start_of_day),
            end_time: format!("{}Z", end_of_day),
            order: "descending".to_string(),
        };

        // Both clients request today's frames
        write1
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("Failed to send request to client 1");
        write2
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("Failed to send request to client 2");

        // Wait and verify both clients receive frames
        // After fix, both should receive new frames pushed by server
        let client1_frames = timeout(std::time::Duration::from_secs(5), async {
            let mut count = 0;
            while let Some(Ok(msg)) = read1.next().await {
                if let Message::Text(text) = msg {
                    if text == "\"keep-alive-text\"" {
                        break;
                    }
                    count += 1;
                }
            }
            count
        })
        .await
        .unwrap_or(0);

        let client2_frames = timeout(std::time::Duration::from_secs(5), async {
            let mut count = 0;
            while let Some(Ok(msg)) = read2.next().await {
                if let Message::Text(text) = msg {
                    if text == "\"keep-alive-text\"" {
                        break;
                    }
                    count += 1;
                }
            }
            count
        })
        .await
        .unwrap_or(0);

        println!("Client 1 received {} frames", client1_frames);
        println!("Client 2 received {} frames", client2_frames);

        // Both clients should receive the same data
        assert!(
            client1_frames > 0 || client2_frames > 0,
            "At least one client should receive frames"
        );
    }

    /// TEST 3: Client should only receive frames within requested time range
    #[tokio::test]
    #[ignore = "requires running server, run with: cargo test stream_frames -- --ignored"]
    async fn test_frames_filtered_by_time_range() {
        let url = "ws://127.0.0.1:3030/stream/frames";

        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        let (mut write, mut read) = ws.split();

        // Request frames for only the last hour
        let now = Utc::now();
        let one_hour_ago = now - Duration::hours(1);

        let request = StreamFramesRequest {
            start_time: one_hour_ago.to_rfc3339(),
            end_time: now.to_rfc3339(),
            order: "descending".to_string(),
        };

        write
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("Failed to send request");

        let frames_received = timeout(std::time::Duration::from_secs(5), async {
            let mut frames = Vec::new();
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg {
                    if text == "\"keep-alive-text\"" {
                        break;
                    }
                    if let Ok(batch) = serde_json::from_str::<Vec<StreamTimeSeriesResponse>>(&text)
                    {
                        for frame in batch {
                            // Verify each frame is within the requested time range
                            let timestamp = chrono::DateTime::parse_from_rfc3339(&frame.timestamp)
                                .expect("Invalid timestamp");
                            assert!(
                                timestamp >= one_hour_ago && timestamp <= now,
                                "Frame timestamp {} is outside requested range",
                                frame.timestamp
                            );
                            frames.push(frame);
                        }
                    }
                }
            }
            frames
        })
        .await;

        println!(
            "Received {} frames within time range",
            frames_received.map(|f| f.len()).unwrap_or(0)
        );
    }

    /// TEST 4: Reconnection should receive fresh data
    #[tokio::test]
    #[ignore = "requires running server, run with: cargo test stream_frames -- --ignored"]
    async fn test_reconnection_receives_fresh_data() {
        let url = "ws://127.0.0.1:3030/stream/frames";

        // First connection
        let (ws1, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect first time");

        let (mut write1, mut read1) = ws1.split();

        let now = Utc::now();
        let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
        let end_of_day = now.date_naive().and_hms_opt(23, 59, 59).unwrap();

        let request = StreamFramesRequest {
            start_time: format!("{}Z", start_of_day),
            end_time: format!("{}Z", end_of_day),
            order: "descending".to_string(),
        };

        write1
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("Failed to send request");

        let first_count = timeout(std::time::Duration::from_secs(5), async {
            let mut count = 0;
            while let Some(Ok(msg)) = read1.next().await {
                if let Message::Text(text) = msg {
                    if text == "\"keep-alive-text\"" {
                        break;
                    }
                    count += 1;
                }
            }
            count
        })
        .await
        .unwrap_or(0);

        // Close first connection
        drop(write1);
        drop(read1);

        // Wait a bit, then reconnect
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Second connection should also receive frames
        let (ws2, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to reconnect");

        let (mut write2, mut read2) = ws2.split();

        write2
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("Failed to send request on reconnect");

        let second_count = timeout(std::time::Duration::from_secs(5), async {
            let mut count = 0;
            while let Some(Ok(msg)) = read2.next().await {
                if let Message::Text(text) = msg {
                    if text == "\"keep-alive-text\"" {
                        break;
                    }
                    count += 1;
                }
            }
            count
        })
        .await
        .unwrap_or(0);

        println!("First connection: {} frames", first_count);
        println!("Second connection: {} frames", second_count);

        // After fix with live push, second connection should have >= frames as first
        // (might have more if new frames were recorded between connections)
        assert!(
            second_count >= first_count || first_count == 0,
            "Reconnection should receive at least as many frames"
        );
    }

    /// TEST 5: Edge case - empty time range should return no frames
    #[tokio::test]
    #[ignore = "requires running server, run with: cargo test stream_frames -- --ignored"]
    async fn test_empty_time_range() {
        let url = "ws://127.0.0.1:3030/stream/frames";

        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        let (mut write, mut read) = ws.split();

        // Request frames for a time range in the far future (no data)
        let future = Utc::now() + Duration::days(365);

        let request = StreamFramesRequest {
            start_time: future.to_rfc3339(),
            end_time: (future + Duration::hours(1)).to_rfc3339(),
            order: "descending".to_string(),
        };

        write
            .send(Message::Text(serde_json::to_string(&request).unwrap()))
            .await
            .expect("Failed to send request");

        let frames_received = timeout(std::time::Duration::from_secs(3), async {
            let mut frames = Vec::new();
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg {
                    if text == "\"keep-alive-text\"" {
                        break;
                    }
                    if let Ok(batch) = serde_json::from_str::<Vec<StreamTimeSeriesResponse>>(&text)
                    {
                        frames.extend(batch);
                    }
                }
            }
            frames
        })
        .await
        .unwrap_or_default();

        assert!(
            frames_received.is_empty(),
            "Future time range should return no frames"
        );
    }
}
