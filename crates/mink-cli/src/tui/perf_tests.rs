//! Reproducible renderer benchmark; run in release with --ignored --nocapture.
use super::*;
use ratatui::{Terminal, backend::TestBackend};
use std::time::Instant;

#[test]
#[ignore = "release TUI benchmark"]
fn tui_release_benchmark() {
    if cfg!(debug_assertions) {
        panic!("run this benchmark with --release");
    }
    println!("messages,width,height,cold_us,warm_p95_us,history_visits,parses");
    for count in [1_000, 10_000] {
        for width in [40, 80, 120, 200] {
            for height in [12, 40] {
                let mut state = TuiState::default();
                for index in 0..count {
                    state.push_line(state::TranscriptItem::new(
                        format!("Message {index}\n\n**Markdown** with 中文 and `code`.\n"),
                        state::TranscriptKind::StreamText,
                    ));
                }
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let cold = Instant::now();
                terminal
                    .draw(|f| render(f, &mut state, TuiMode::Full))
                    .unwrap();
                while state.cache.rebuild_next.is_some() || !state.cache.pending.is_empty() {
                    terminal
                        .draw(|f| render(f, &mut state, TuiMode::Full))
                        .unwrap();
                }
                let cold = cold.elapsed().as_micros();
                let visits = state.cache.visited;
                let parses = state.cache.parsed;
                let mut samples = Vec::new();
                for index in 0..100 {
                    state.input.revision += 1;
                    state.input.buf = format!("draft {index}");
                    state.input.cursor = state.input.buf.len();
                    let start = Instant::now();
                    terminal
                        .draw(|f| render(f, &mut state, TuiMode::Full))
                        .unwrap();
                    samples.push(start.elapsed().as_micros());
                }
                samples.sort_unstable();
                assert_eq!(state.cache.visited, visits);
                println!(
                    "{count},{width},{height},{cold},{},{},{}",
                    samples[94],
                    state.cache.visited - visits,
                    state.cache.parsed - parses
                );
            }
        }
    }
}

#[test]
#[ignore = "release TUI stress benchmark"]
fn tui_release_stress_benchmark() {
    if cfg!(debug_assertions) {
        panic!("run with --release");
    }
    for width in [40, 80, 120, 200] {
        let mut state = TuiState::default();
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        state.artifact_detail = Some(state::ArtifactDetail {
            id: "a0001".into(),
            content: "artifact line 中文\n".repeat(12_480),
            truncated: true,
        });
        state.view = state::View::Artifact { scroll: 0 };
        terminal
            .draw(|f| render(f, &mut state, TuiMode::Full))
            .unwrap();
        let parses = state.cache.detail_parses;
        let start = Instant::now();
        for scroll in 1..100 {
            state.view = state::View::Artifact { scroll };
            terminal
                .draw(|f| render(f, &mut state, TuiMode::Full))
                .unwrap();
        }
        assert_eq!(parses, state.cache.detail_parses);
        println!(
            "detail,{width},scroll_mean_us={},parses={}",
            start.elapsed().as_micros() / 99,
            state.cache.detail_parses - parses
        );
        state.view = state::View::Main;
        state.apply(&TuiSignal::Text("```text\nSTART\n".into()));
        state.promote_stable_stream_prefix();
        let start = Instant::now();
        for _ in 0..256 {
            state.apply(&TuiSignal::Text("x".repeat(1023) + "\n"));
            state.promote_stable_stream_prefix();
            terminal
                .draw(|f| render(f, &mut state, TuiMode::Full))
                .unwrap();
        }
        println!(
            "long_code,{width},frame_mean_us={},line_parses={}",
            start.elapsed().as_micros() / 256,
            state.cache.stream_parses
        );
        for n in 0..32 {
            state.apply(&TuiSignal::SubAgentStatus {
                session_id: format!("sub_{n}"),
                status: "running".into(),
                in_tokens: 0,
                out_tokens: 0,
            });
            state.apply(&TuiSignal::SubAgentStream {
                session_id: format!("sub_{n}"),
                kind: crate::ui::SubAgentStreamKind::Thinking,
                content: "thinking\n".repeat(128),
            });
            let input = serde_json::json!({"request_id":format!("input-{n}"),"text":"pending input","target_turn_id":"turn","attachment_ids":[]});
            state.inputs.push(serde_json::from_value(serde_json::json!({"input_id":format!("input-{n}"),"revision":1,"turn_id":"turn","status":"pending","guidance":true,"input":input,"original":input})).unwrap());
        }
        let start = Instant::now();
        terminal
            .draw(|f| render(f, &mut state, TuiMode::Full))
            .unwrap();
        println!(
            "subagents_and_inbox,{width},frame_us={}",
            start.elapsed().as_micros()
        );
    }
    // Saturate the same bounded forwarding bridge used by the UI, retaining
    // enqueue timestamps to measure backlog age separately from frame work.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut receiver = async_receiver(rx);
        let producer = std::thread::spawn(move || {
            for index in 0..10_000 {
                tx.send((index, Instant::now(), TuiSignal::Text("line\n\n".into())))
                    .unwrap();
            }
        });
        let mut ages = Vec::new();
        let mut state = TuiState::default();
        let mut expected = 0;
        while let Some(first) = receiver.recv().await {
            let mut batch = vec![first];
            while batch.len() < 512 {
                match receiver.try_recv() {
                    Ok(event) => batch.push(event),
                    Err(_) => break,
                }
            }
            let signals = batch
                .into_iter()
                .map(|(index, queued, signal)| {
                    assert_eq!(index, expected);
                    expected += 1;
                    ages.push(queued.elapsed().as_micros());
                    signal
                })
                .collect();
            signal::apply_signals(signals, &mut state, TuiMode::Full);
            tokio::task::yield_now().await;
        }
        producer.join().unwrap();
        assert_eq!(expected, 10_000);
        ages.sort_unstable();
        println!(
            "signal_backlog,events={expected},age_p95_us={},age_max_us={}",
            ages[9499], ages[9999]
        );
    });
}

#[test]
#[ignore = "release remaining-tail audit benchmark"]
fn tui_release_remaining_tail_benchmark() {
    if cfg!(debug_assertions) {
        panic!("run with --release");
    }
    for target_bytes in [256 * 1024, 1024 * 1024] {
        for kind in ["paragraph", "table"] {
            let mut state = TuiState::default();
            let body = if kind == "table" {
                "| a | b |\n|---|---|\n".to_string()
                    + &"| column one | column two |\n".repeat(target_bytes / 28)
            } else {
                "word ".repeat(target_bytes / 5)
            };
            state.apply(&TuiSignal::Text(body));
            state.promote_stable_stream_prefix();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|f| render(f, &mut state, TuiMode::Full))
                .unwrap();
            let mut samples = Vec::new();
            for _ in 0..20 {
                state.apply(&TuiSignal::Text(
                    if kind == "table" {
                        "| another | row |\n"
                    } else {
                        "more words "
                    }
                    .into(),
                ));
                state.promote_stable_stream_prefix();
                let start = Instant::now();
                terminal
                    .draw(|f| render(f, &mut state, TuiMode::Full))
                    .unwrap();
                samples.push(start.elapsed().as_micros());
            }
            samples.sort_unstable();
            println!(
                "remaining_tail,{kind},update_p95_us={},bytes={}",
                samples[18],
                state.stream_line.len()
            );
        }
    }
}
