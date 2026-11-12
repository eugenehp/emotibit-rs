//! Terminal oscilloscope.
//!
//! One chart per signal on a shared time window, with the Oscilloscope's header bar
//! (device, battery, recording, power mode, clipping/overflow, lost packets). Handles several
//! devices at once: `Tab` switches between them.
//!
//! Keys: `←`/`→` time window, `Space` pause, `r` start/stop recording on the device's SD card,
//! `n` add a note, `m` cycle power mode (never hibernate from the keyboard), `Tab` next device,
//! `q`/`Esc`/`Ctrl-C` quit.
//!
//! Rendering ([`render`]) and key handling ([`handle_key`]) are plain functions of state, so
//! they are tested without a terminal.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::emotibit_client::EmotiBitHandle;
use crate::output::Outputs;
use crate::scope::Scope;
use crate::status::PowerMode;
use crate::types::{channel, CHANNELS, EmotiBitEvent};

/// Modes the `m` key cycles through. Hibernate is deliberately absent: it powers the device
/// down and it can only be woken by its button.
const CYCLE_MODES: [PowerMode; 4] = [PowerMode::Normal, PowerMode::Low, PowerMode::MaxLow, PowerMode::WirelessOff];

/// One connected device's display state.
pub struct DeviceView {
    pub id: String,
    pub scope: Scope,
    pub connected: bool,
}

impl DeviceView {
    pub fn new(id: &str) -> Self {
        Self { id: id.to_string(), scope: Scope::new(), connected: true }
    }
}

/// UI state that is not about a device.
#[derive(Default)]
pub struct Ui {
    pub selected: usize,
    /// `Some` while the user is typing a note.
    pub note: Option<String>,
    pub message: Option<(String, Instant)>,
}

/// What a key press asks the event loop to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    ToggleRecord,
    SendNote(String),
    CycleMode,
}

pub fn handle_key(ui: &mut Ui, devices: &mut [DeviceView], key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    // Typing a note swallows everything except Enter/Esc/Backspace.
    if let Some(text) = ui.note.as_mut() {
        match key.code {
            KeyCode::Enter => {
                let note = ui.note.take().unwrap_or_default();
                return if note.trim().is_empty() { Action::None } else { Action::SendNote(note) };
            }
            KeyCode::Esc => ui.note = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => {}
        }
        return Action::None;
    }
    let Some(dev) = devices.get_mut(ui.selected) else { return Action::None };
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Right => dev.scope.increment_window(),
        KeyCode::Left => dev.scope.decrement_window(),
        KeyCode::Char(' ') => dev.scope.toggle_pause(),
        KeyCode::Char('r') => return Action::ToggleRecord,
        KeyCode::Char('m') => return Action::CycleMode,
        KeyCode::Char('n') => ui.note = Some(String::new()),
        KeyCode::Tab if !devices.is_empty() => ui.selected = (ui.selected + 1) % devices.len(),
        _ => {}
    }
    Action::None
}

fn tag_color(tag: &str) -> Color {
    match tag {
        "PR" => Color::LightRed,
        "PI" => Color::Magenta,
        "PG" => Color::Green,
        "HR" => Color::Red,
        "EA" | "EL" | "ER" | "SA" | "SR" | "SF" => Color::Blue,
        "AX" | "GX" | "MX" => Color::LightYellow,
        "AY" | "GY" | "MY" => Color::LightGreen,
        "AZ" | "GZ" | "MZ" => Color::LightBlue,
        "T0" | "T1" => Color::Yellow,
        "TH" => Color::LightRed,
        _ => Color::Gray,
    }
}

fn fmt(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.3}")
    }
}

/// Channels in the Oscilloscope's order, then anything else that has data.
fn ordered_tags(scope: &Scope) -> Vec<&'static str> {
    let have = scope.tags();
    let mut out: Vec<&'static str> = CHANNELS.iter().map(|c| c.tag).filter(|t| have.contains(t)).collect();
    let extra: Vec<&'static str> = have.into_iter().filter(|t| !out.contains(t)).collect();
    out.extend(extra);
    out
}

pub fn render(frame: &mut Frame, devices: &[DeviceView], ui: &Ui) {
    let area = frame.area();
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(3), Constraint::Length(1)]).areas(area);

    let Some(dev) = devices.get(ui.selected) else {
        frame.render_widget(Paragraph::new("no device"), area);
        return;
    };
    render_header(frame, header, dev, devices.len(), ui.selected);
    render_charts(frame, body, dev);
    render_footer(frame, footer, ui, dev);
}

fn render_header(frame: &mut Frame, area: Rect, dev: &DeviceView, n: usize, selected: usize) {
    let s = dev.scope.status();
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let bad = |v: u64| if v > 0 { Style::default().fg(Color::Red) } else { Style::default() };
    let rec = match s.recording {
        Some(true) => Span::styled("● RECORDING", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        _ => Span::styled("○ not recording", Style::default().fg(Color::DarkGray)),
    };
    let device = if n > 1 { format!("{} ({}/{})", dev.id, selected + 1, n) } else { dev.id.clone() };
    let line1 = Line::from(vec![
        Span::styled(" EmotiBit ", bold),
        Span::raw(device),
        Span::raw("   battery "),
        Span::styled(s.battery_percent.map_or("–".into(), |b| format!("{b:.0}%")), bold),
        Span::raw("   "),
        rec,
        Span::raw(format!("   power {}", s.power_mode.map_or("–", |m| m.label()))),
        if dev.connected { Span::raw("") } else { Span::styled("   DISCONNECTED", Style::default().fg(Color::Red)) },
    ]);
    let h = dev.scope.health();
    let line2 = Line::from(vec![
        Span::raw(" clipping "),
        Span::styled(s.clipping_events.to_string(), bad(s.clipping_events as u64)),
        Span::raw("  overflow "),
        Span::styled(s.overflow_events.to_string(), bad(s.overflow_events as u64)),
        Span::raw("  seq gaps "),
        Span::styled(h.total_lost().to_string(), bad(h.total_lost())),
        Span::raw("  underruns "),
        Span::styled(h.total_underruns().to_string(), bad(h.total_underruns())),
        Span::raw(format!("  window {} s", dev.scope.window_s())),
        if dev.scope.is_paused() { Span::styled("  PAUSED", Style::default().fg(Color::Yellow)) } else { Span::raw("") },
    ]);
    frame.render_widget(Paragraph::new(vec![line1, line2]), area);
}

fn render_charts(frame: &mut Frame, area: Rect, dev: &DeviceView) {
    let tags = ordered_tags(&dev.scope);
    if tags.is_empty() {
        frame.render_widget(Paragraph::new(" waiting for data…").style(Style::default().fg(Color::DarkGray)), area);
        return;
    }
    let cols = if area.width >= 100 { 2 } else { 1 };
    let rows = tags.len().div_ceil(cols);
    let row_areas = Layout::vertical((0..rows).map(|_| Constraint::Ratio(1, rows as u32))).split(area);
    let win = dev.scope.window_s();
    for (i, tag) in tags.iter().enumerate() {
        let col_areas = Layout::horizontal((0..cols).map(|_| Constraint::Ratio(1, cols as u32))).split(row_areas[i / cols]);
        let cell = col_areas[i % cols];
        let (lo, hi) = dev.scope.y_range(tag);
        let data = dev.scope.window(tag);
        let ch = channel(tag);
        let name = ch.map_or(*tag, |c| c.name);
        let unit = ch.map_or("", |c| c.unit);
        let last = dev.scope.last_value(tag).map_or("–".into(), fmt);
        let color = tag_color(tag);
        let ds = Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(color))
            .data(&data);
        let chart = Chart::new(vec![ds])
            .block(Block::bordered().border_style(Style::default().fg(Color::DarkGray)).title(Span::styled(
                format!(" {name}  {last} {unit} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )))
            .x_axis(Axis::default().bounds([-win, 0.0]))
            .y_axis(Axis::default().bounds([lo, hi]).labels(vec![fmt(lo), fmt(hi)]));
        frame.render_widget(chart, cell);
    }
}

fn render_footer(frame: &mut Frame, area: Rect, ui: &Ui, dev: &DeviceView) {
    let line = if let Some(note) = &ui.note {
        Line::from(vec![Span::styled(" note: ", Style::default().fg(Color::Yellow)), Span::raw(format!("{note}▏")), Span::styled("   Enter send · Esc cancel", Style::default().fg(Color::DarkGray))])
    } else if let Some((msg, at)) = ui.message.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4)) {
        let _ = at;
        Line::from(Span::styled(format!(" {msg}"), Style::default().fg(Color::Yellow)))
    } else {
        let _ = dev;
        Line::from(Span::styled(
            " ←/→ window · space pause · r record · n note · m power mode · Tab device · q quit",
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(line), area);
}

enum Msg {
    Event(usize, EmotiBitEvent),
    Key(KeyEvent),
    Closed(usize),
}

/// Run the terminal UI for the given connections until the user quits or all devices end.
pub async fn run(connections: Vec<(mpsc::Receiver<EmotiBitEvent>, EmotiBitHandle)>, mut outputs: Vec<Option<Outputs>>) -> Result<()> {
    let handles: Vec<EmotiBitHandle> = connections.iter().map(|(_, h)| h.clone()).collect();
    let mut devices: Vec<DeviceView> = handles.iter().map(|h| DeviceView::new(&h.device().id)).collect();
    outputs.resize_with(devices.len(), || None);
    let mut ui = Ui::default();

    let (tx, mut rx) = mpsc::channel::<Msg>(8192);
    for (i, (mut events, _)) in connections.into_iter().enumerate() {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if tx.send(Msg::Event(i, ev)).await.is_err() {
                    return;
                }
            }
            let _ = tx.send(Msg::Closed(i)).await;
        });
    }
    // crossterm's reader blocks, so it gets its own thread.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (tx, stop) = (tx.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if event::poll(Duration::from_millis(100)).unwrap_or(false)
                    && let Ok(Event::Key(k)) = event::read()
                        && tx.blocking_send(Msg::Key(k)).is_err() {
                            return;
                        }
            }
        });
    }

    let mut terminal = ratatui::init();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = tick.tick() => { terminal.draw(|f| render(f, &devices, &ui))?; }
                msg = rx.recv() => match msg {
                    Some(Msg::Event(i, ev)) => {
                        if let Some(o) = outputs[i].as_mut() { o.on_event(&ev); }
                        match ev {
                            EmotiBitEvent::Sensor(r) => devices[i].scope.push_reading(&r),
                            EmotiBitEvent::Message(p) => devices[i].scope.push_packet(p, crate::parse::now_ms()),
                            EmotiBitEvent::Disconnected => devices[i].connected = false,
                            _ => {}
                        }
                    }
                    Some(Msg::Closed(i)) => {
                        devices[i].connected = false;
                        if devices.iter().all(|d| !d.connected) {
                            ui.message = Some(("all devices disconnected; press q to quit".into(), Instant::now()));
                        }
                    }
                    Some(Msg::Key(k)) => {
                        let h = &handles[ui.selected.min(handles.len().saturating_sub(1))];
                        let note = |m: &str, ui: &mut Ui| ui.message = Some((m.to_string(), Instant::now()));
                        match handle_key(&mut ui, &mut devices, k) {
                            Action::Quit => break,
                            Action::ToggleRecord => {
                                let recording = devices[ui.selected].scope.status().recording == Some(true);
                                let r = if recording { h.record_end().await } else { h.record_begin().await };
                                if let Err(e) = r { note(&format!("record: {e}"), &mut ui); }
                            }
                            Action::SendNote(text) => match h.user_note(&text).await {
                                Ok(()) => note("note sent", &mut ui),
                                Err(e) => note(&format!("note: {e}"), &mut ui),
                            },
                            Action::CycleMode => {
                                let cur = devices[ui.selected].scope.status().power_mode;
                                let next = cur
                                    .and_then(|c| CYCLE_MODES.iter().position(|m| *m == c))
                                    .map_or(CYCLE_MODES[0], |i| CYCLE_MODES[(i + 1) % CYCLE_MODES.len()]);
                                match h.set_power_mode(next).await {
                                    Ok(()) => note(&format!("power mode → {}", next.label()), &mut ui),
                                    Err(e) => note(&format!("power mode: {e}"), &mut ui),
                                }
                            }
                            Action::None => {}
                        }
                    }
                    None => break,
                },
            }
        }
        Ok(())
    }
    .await;

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    ratatui::restore();
    for h in &handles {
        let _ = h.disconnect().await;
    }
    for o in outputs.into_iter().flatten() {
        o.stop();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SensorReading;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn device_with_data() -> DeviceView {
        let mut d = DeviceView::new("MD-V6-0000356");
        for i in 0..20u32 {
            for tag in ["AX", "PG", "EA"] {
                d.scope.push_reading(&SensorReading {
                    channel: channel(tag).unwrap(),
                    packet_number: i as u16,
                    device_timestamp_ms: i * 100,
                    received_at_ms: i as f64 * 100.0,
                    samples: vec![(i as f64).sin(), (i as f64).cos()],
                });
            }
        }
        d
    }

    fn screen(devices: &[DeviceView], ui: &Ui, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, devices, ui)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn renders_header_and_a_chart_per_signal() {
        let devices = vec![device_with_data()];
        let s = screen(&devices, &Ui::default(), 120, 30);
        assert!(s.contains("MD-V6-0000356"));
        assert!(s.contains("not recording"));
        for name in ["ACC:X", "PPG:GRN", "EDA"] {
            assert!(s.contains(name), "missing {name} in:\n{s}");
        }
    }

    #[test]
    fn empty_device_waits_for_data() {
        let s = screen(&[DeviceView::new("D")], &Ui::default(), 80, 12);
        assert!(s.contains("waiting for data"));
    }

    #[test]
    fn keys_drive_the_scope() {
        let mut devices = vec![device_with_data()];
        let mut ui = Ui::default();
        assert_eq!(devices[0].scope.window_s(), 10.0);
        handle_key(&mut ui, &mut devices, key(KeyCode::Left));
        assert_eq!(devices[0].scope.window_s(), 5.0);
        handle_key(&mut ui, &mut devices, key(KeyCode::Right));
        assert_eq!(devices[0].scope.window_s(), 10.0);
        handle_key(&mut ui, &mut devices, key(KeyCode::Char(' ')));
        assert!(devices[0].scope.is_paused());
        assert_eq!(handle_key(&mut ui, &mut devices, key(KeyCode::Char('r'))), Action::ToggleRecord);
        assert_eq!(handle_key(&mut ui, &mut devices, key(KeyCode::Char('m'))), Action::CycleMode);
        assert_eq!(handle_key(&mut ui, &mut devices, key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(
            handle_key(&mut ui, &mut devices, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn note_entry_captures_keys_including_q() {
        let mut devices = vec![device_with_data()];
        let mut ui = Ui::default();
        handle_key(&mut ui, &mut devices, key(KeyCode::Char('n')));
        for c in "hi q".chars() {
            assert_eq!(handle_key(&mut ui, &mut devices, key(KeyCode::Char(c))), Action::None);
        }
        handle_key(&mut ui, &mut devices, key(KeyCode::Backspace));
        assert_eq!(
            handle_key(&mut ui, &mut devices, key(KeyCode::Enter)),
            Action::SendNote("hi ".into())
        );
        assert!(ui.note.is_none());
        // Esc cancels without sending.
        handle_key(&mut ui, &mut devices, key(KeyCode::Char('n')));
        handle_key(&mut ui, &mut devices, key(KeyCode::Char('x')));
        assert_eq!(handle_key(&mut ui, &mut devices, key(KeyCode::Esc)), Action::None);
        assert!(ui.note.is_none());
    }

    #[test]
    fn tab_switches_devices() {
        let mut devices = vec![DeviceView::new("A"), DeviceView::new("B")];
        let mut ui = Ui::default();
        handle_key(&mut ui, &mut devices, key(KeyCode::Tab));
        assert_eq!(ui.selected, 1);
        assert!(screen(&devices, &ui, 80, 10).contains("B (2/2)"));
        handle_key(&mut ui, &mut devices, key(KeyCode::Tab));
        assert_eq!(ui.selected, 0);
    }

    #[test]
    fn power_mode_cycle_never_includes_hibernate() {
        assert!(!CYCLE_MODES.contains(&PowerMode::Hibernate));
    }
}
