use std::{
    io::{stdout, Write},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent},
    execute, terminal,
};
use eyre::Result;
use futures_util::FutureExt;
use semver::Version;
use skim::SkimItem;
use unicode_width::UnicodeWidthStr;

use atuin_client::{
    database::Database,
    database::{current_context, Context},
    history::History,
    settings::{ExitMode, FilterMode, SearchMode, Settings},
};

use super::{
    cursor::Cursor,
    history_list::{HistoryList, ListState, PREFIX_LENGTH},
};
use crate::{
    tui::{
        backend::{Backend, CrosstermBackend},
        layout::{Alignment, Constraint, Direction, Layout},
        style::{Color, Modifier, Style},
        text::{Span, Spans, Text},
        widgets::{Block, BorderType, Borders, Paragraph},
        Frame, Terminal,
    },
    VERSION,
};

const RETURN_ORIGINAL: usize = usize::MAX;
const RETURN_QUERY: usize = usize::MAX - 1;

struct State {
    history_count: i64,
    update_needed: Option<Version>,
    results_state: ListState,
    search: SearchState,

    // only allocated if using skim
    all_history: Vec<Arc<HistoryWrapper>>,
}

pub struct SearchState {
    pub input: Cursor,
    pub filter_mode: FilterMode,
    pub search_mode: SearchMode,
    /// Store if the user has _just_ changed the search mode.
    /// If so, we change the UI to show the search mode instead
    /// of the filter mode until user starts typing again.
    switched_search_mode: bool,
    pub context: Context,
}

impl State {
    async fn query_results(&mut self, db: &mut impl Database) -> Result<Vec<Arc<HistoryWrapper>>> {
        let i = self.search.input.as_str();
        let results = if i.is_empty() {
            db.list(
                self.search.filter_mode,
                &self.search.context,
                Some(200),
                true,
            )
            .await?
            .into_iter()
            .map(|history| HistoryWrapper { history, count: 1 })
            .map(Arc::new)
            .collect::<Vec<_>>()
        } else if self.search.search_mode == SearchMode::Skim {
            if self.all_history.is_empty() {
                self.all_history = db
                    .all_with_count()
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|(history, count)| HistoryWrapper { history, count })
                    .map(Arc::new)
                    .collect::<Vec<_>>();
            }

            super::skim_impl::fuzzy_search(&self.search, &self.all_history).await
        } else {
            db.search(
                self.search.search_mode,
                self.search.filter_mode,
                &self.search.context,
                i,
                Some(200),
                None,
                None,
            )
            .await?
            .into_iter()
            .map(|history| HistoryWrapper { history, count: 1 })
            .map(Arc::new)
            .collect::<Vec<_>>()
        };

        self.results_state.select(0);
        Ok(results)
    }

    fn handle_input(&mut self, settings: &Settings, input: &Event, len: usize) -> Option<usize> {
        match input {
            Event::Key(k) => self.handle_key_input(settings, k, len),
            Event::Mouse(m) => self.handle_mouse_input(*m, len),
            Event::Paste(d) => self.handle_paste_input(d),
            _ => None,
        }
    }

    fn handle_mouse_input(&mut self, input: MouseEvent, len: usize) -> Option<usize> {
        match input.kind {
            event::MouseEventKind::ScrollDown => {
                let i = self.results_state.selected().saturating_sub(1);
                self.results_state.select(i);
            }
            event::MouseEventKind::ScrollUp => {
                let i = self.results_state.selected() + 1;
                self.results_state.select(i.min(len - 1));
            }
            _ => {}
        }
        None
    }

    fn handle_paste_input(&mut self, input: &str) -> Option<usize> {
        for i in input.chars() {
            self.search.input.insert(i);
        }
        None
    }

    #[allow(clippy::too_many_lines)]
    fn handle_key_input(
        &mut self,
        settings: &Settings,
        input: &KeyEvent,
        len: usize,
    ) -> Option<usize> {
        if input.kind == event::KeyEventKind::Release {
            return None;
        }

        let ctrl = input.modifiers.contains(KeyModifiers::CONTROL);
        let alt = input.modifiers.contains(KeyModifiers::ALT);
        // reset the state, will be set to true later if user really did change it
        self.search.switched_search_mode = false;
        match input.code {
            KeyCode::Char('c' | 'd' | 'g') if ctrl => return Some(RETURN_ORIGINAL),
            KeyCode::Esc => {
                return Some(match settings.exit_mode {
                    ExitMode::ReturnOriginal => RETURN_ORIGINAL,
                    ExitMode::ReturnQuery => RETURN_QUERY,
                })
            }
            KeyCode::Enter => {
                return Some(self.results_state.selected());
            }
            KeyCode::Char(c @ '1'..='9') if alt => {
                let c = c.to_digit(10)? as usize;
                return Some(self.results_state.selected() + c);
            }
            KeyCode::Left if ctrl => self
                .search
                .input
                .prev_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Left => {
                self.search.input.left();
            }
            KeyCode::Char('h') if ctrl => {
                self.search.input.left();
            }
            KeyCode::Right if ctrl => self
                .search
                .input
                .next_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Right => self.search.input.right(),
            KeyCode::Char('l') if ctrl => self.search.input.right(),
            KeyCode::Char('a') if ctrl => self.search.input.start(),
            KeyCode::Home => self.search.input.start(),
            KeyCode::Char('e') if ctrl => self.search.input.end(),
            KeyCode::End => self.search.input.end(),
            KeyCode::Backspace if ctrl => self
                .search
                .input
                .remove_prev_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Backspace => {
                self.search.input.back();
            }
            KeyCode::Delete if ctrl => self
                .search
                .input
                .remove_next_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Delete => {
                self.search.input.remove();
            }
            KeyCode::Char('w') if ctrl => {
                // remove the first batch of whitespace
                while matches!(self.search.input.back(), Some(c) if c.is_whitespace()) {}
                while self.search.input.left() {
                    if self.search.input.char().unwrap().is_whitespace() {
                        self.search.input.right(); // found whitespace, go back right
                        break;
                    }
                    self.search.input.remove();
                }
            }
            KeyCode::Char('u') if ctrl => self.search.input.clear(),
            KeyCode::Char('r') if ctrl => {
                pub static FILTER_MODES: [FilterMode; 4] = [
                    FilterMode::Global,
                    FilterMode::Host,
                    FilterMode::Session,
                    FilterMode::Directory,
                ];
                let i = self.search.filter_mode as usize;
                let i = (i + 1) % FILTER_MODES.len();
                self.search.filter_mode = FILTER_MODES[i];
            }
            KeyCode::Char('s') if ctrl => {
                self.search.switched_search_mode = true;
                self.search.search_mode = self.search.search_mode.next(settings);
            }
            KeyCode::Down if self.results_state.selected() == 0 => return Some(RETURN_ORIGINAL),
            KeyCode::Down => {
                let i = self.results_state.selected().saturating_sub(1);
                self.results_state.select(i);
            }
            KeyCode::Char('n' | 'j') if ctrl => {
                let i = self.results_state.selected().saturating_sub(1);
                self.results_state.select(i);
            }
            KeyCode::Up => {
                let i = self.results_state.selected() + 1;
                self.results_state.select(i.min(len - 1));
            }
            KeyCode::Char('p' | 'k') if ctrl => {
                let i = self.results_state.selected() + 1;
                self.results_state.select(i.min(len - 1));
            }
            KeyCode::Char(c) => self.search.input.insert(c),
            KeyCode::PageDown => {
                let scroll_len = self.results_state.max_entries() - settings.scroll_context_lines;
                let i = self.results_state.selected().saturating_sub(scroll_len);
                self.results_state.select(i);
            }
            KeyCode::PageUp => {
                let scroll_len = self.results_state.max_entries() - settings.scroll_context_lines;
                let i = self.results_state.selected() + scroll_len;
                self.results_state.select(i.min(len - 1));
            }
            _ => {}
        };

        None
    }

    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::bool_to_int_with_if)]
    fn draw<T: Backend>(
        &mut self,
        f: &mut Frame<'_, T>,
        results: &[Arc<HistoryWrapper>],
        compact: bool,
        show_preview: bool,
    ) {
        let border_size = if compact { 0 } else { 1 };
        let preview_width = f.size().width - 2;
        let preview_height = if show_preview {
            let longest_command = results
                .iter()
                .max_by(|h1, h2| h1.command.len().cmp(&h2.command.len()));
            longest_command.map_or(0, |v| {
                std::cmp::min(
                    4,
                    (v.command.len() as u16 + preview_width - 1 - border_size)
                        / (preview_width - border_size),
                )
            }) + border_size * 2
        } else if compact {
            0
        } else {
            1
        };
        let show_help = !compact || f.size().height > 1;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .horizontal_margin(1)
            .constraints(
                [
                    Constraint::Length(if show_help { 1 } else { 0 }),
                    Constraint::Min(1),
                    Constraint::Length(1 + border_size),
                    Constraint::Length(preview_height),
                ]
                .as_ref(),
            )
            .split(f.size());

        let header_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                [
                    Constraint::Ratio(1, 3),
                    Constraint::Ratio(1, 3),
                    Constraint::Ratio(1, 3),
                ]
                .as_ref(),
            )
            .split(chunks[0]);

        let title = self.build_title();
        f.render_widget(title, header_chunks[0]);

        let help = self.build_help();
        f.render_widget(help, header_chunks[1]);

        let stats = self.build_stats();
        f.render_widget(stats, header_chunks[2]);

        let results_list = Self::build_results_list(compact, results);
        f.render_stateful_widget(results_list, chunks[1], &mut self.results_state);

        let input = self.build_input(compact, chunks[2].width.into());
        f.render_widget(input, chunks[2]);

        let preview = self.build_preview(results, compact, preview_width, chunks[3].width.into());
        f.render_widget(preview, chunks[3]);

        let extra_width = UnicodeWidthStr::width(self.search.input.substring());

        let cursor_offset = if compact { 0 } else { 1 };
        f.set_cursor(
            // Put cursor past the end of the input text
            chunks[2].x + extra_width as u16 + PREFIX_LENGTH + 1 + cursor_offset,
            chunks[2].y + cursor_offset,
        );
    }

    fn build_title(&mut self) -> Paragraph {
        let title = if self.update_needed.is_some() {
            let version = self.update_needed.clone().unwrap();

            Paragraph::new(Text::from(Span::styled(
                format!(" Atuin v{VERSION} - UPDATE AVAILABLE {version}"),
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
            )))
        } else {
            Paragraph::new(Text::from(Span::styled(
                format!(" Atuin v{VERSION}"),
                Style::default().add_modifier(Modifier::BOLD),
            )))
        };
        title
    }

    #[allow(clippy::unused_self)]
    fn build_help(&mut self) -> Paragraph {
        let help = Paragraph::new(Text::from(Spans::from(vec![
            Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" to exit"),
        ])))
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
        help
    }

    fn build_stats(&mut self) -> Paragraph {
        let stats = Paragraph::new(Text::from(Span::raw(format!(
            "history count: {}",
            self.history_count,
        ))))
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Right);
        stats
    }

    fn build_results_list(compact: bool, results: &[Arc<HistoryWrapper>]) -> HistoryList {
        let results_list = if compact {
            HistoryList::new(results)
        } else {
            HistoryList::new(results).block(
                Block::default()
                    .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded),
            )
        };
        results_list
    }

    fn build_input(&mut self, compact: bool, chunk_width: usize) -> Paragraph {
        let (pref, main, main_width) = if self.search.switched_search_mode {
            ("SRCH:", self.search.search_mode.as_str(), 9)
        } else {
            ("", self.search.filter_mode.as_str(), 14)
        };
        let input = format!(
            "[{pref:>}{main:^main_width$}] {}",
            self.search.input.as_str(),
        );
        let input = if compact {
            Paragraph::new(input)
        } else {
            Paragraph::new(input).block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded)
                    .title(format!("{:─>width$}", "", width = chunk_width - 2)),
            )
        };
        input
    }

    fn build_preview(
        &mut self,
        results: &[Arc<HistoryWrapper>],
        compact: bool,
        preview_width: u16,
        chunk_width: usize,
    ) -> Paragraph {
        let selected = self.results_state.selected();
        let command = if results.is_empty() {
            String::new()
        } else {
            use itertools::Itertools as _;
            let s = &results[selected].command;
            s.char_indices()
                .step_by(preview_width.into())
                .map(|(i, _)| i)
                .chain(Some(s.len()))
                .tuple_windows()
                .map(|(a, b)| &s[a..b])
                .join("\n")
        };
        let preview = if compact {
            Paragraph::new(command).style(Style::default().fg(Color::DarkGray))
        } else {
            Paragraph::new(command).block(
                Block::default()
                    .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded)
                    .title(format!("{:─>width$}", "", width = chunk_width - 2)),
            )
        };
        preview
    }
}

struct Stdout {
    stdout: std::io::Stdout,
}

impl Stdout {
    pub fn new() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            event::EnableMouseCapture,
            event::EnableBracketedPaste
        )?;
        Ok(Self { stdout })
    }
}

impl Drop for Stdout {
    fn drop(&mut self) {
        execute!(
            self.stdout,
            terminal::LeaveAlternateScreen,
            event::DisableMouseCapture,
            event::DisableBracketedPaste
        )
        .unwrap();
        terminal::disable_raw_mode().unwrap();
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stdout.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stdout.flush()
    }
}

pub struct HistoryWrapper {
    history: History,
    pub count: i32,
}
impl Deref for HistoryWrapper {
    type Target = History;

    fn deref(&self) -> &Self::Target {
        &self.history
    }
}
impl SkimItem for HistoryWrapper {
    fn text(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed(self.history.command.as_str())
    }
}

// this is a big blob of horrible! clean it up!
// for now, it works. But it'd be great if it were more easily readable, and
// modular. I'd like to add some more stats and stuff at some point
#[allow(clippy::cast_possible_truncation)]
pub async fn history(
    query: &[String],
    settings: &Settings,
    db: &mut impl Database,
) -> Result<String> {
    let stdout = Stdout::new()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut input = Cursor::from(query.join(" "));
    // Put the cursor at the end of the query by default
    input.end();

    let settings2 = settings.clone();
    let update_needed = tokio::spawn(async move { settings2.needs_update().await }).fuse();
    tokio::pin!(update_needed);

    let context = current_context();

    let mut app = State {
        history_count: db.history_count().await?,
        results_state: ListState::default(),
        update_needed: None,
        search: SearchState {
            input,
            context,
            filter_mode: if settings.shell_up_key_binding {
                settings
                    .filter_mode_shell_up_key_binding
                    .unwrap_or(settings.filter_mode)
            } else {
                settings.filter_mode
            },
            search_mode: settings.search_mode,
            switched_search_mode: false,
        },
        all_history: Vec::new(),
    };

    let mut results = app.query_results(db).await?;

    let index = 'render: loop {
        let compact = match settings.style {
            atuin_client::settings::Style::Auto => {
                terminal.size().map(|size| size.height < 14).unwrap_or(true)
            }
            atuin_client::settings::Style::Compact => true,
            atuin_client::settings::Style::Full => false,
        };
        terminal.draw(|f| app.draw(f, &results, compact, settings.show_preview))?;

        let initial_input = app.search.input.as_str().to_owned();
        let initial_filter_mode = app.search.filter_mode;
        let initial_search_mode = app.search.search_mode;

        let event_ready = tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(250)));

        tokio::select! {
            event_ready = event_ready => {
                if event_ready?? {
                    loop {
                        if let Some(i) = app.handle_input(settings, &event::read()?, results.len()) {
                            break 'render i;
                        }
                        if !event::poll(Duration::ZERO)? {
                            break;
                        }
                    }
                }
            }
            update_needed = &mut update_needed => {
                app.update_needed = update_needed?;
            }
        }

        if initial_input != app.search.input.as_str()
            || initial_filter_mode != app.search.filter_mode
            || initial_search_mode != app.search.search_mode
        {
            results = app.query_results(db).await?;
        }
    };
    if index < results.len() {
        // index is in bounds so we return that entry
        Ok(results.swap_remove(index).command.clone())
    } else if index == RETURN_ORIGINAL {
        Ok(String::new())
    } else {
        // Either:
        // * index == RETURN_QUERY, in which case we should return the input
        // * out of bounds -> usually implies no selected entry so we return the input
        Ok(app.search.input.into_inner())
    }
}
