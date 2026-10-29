//! The page's downloads: where each file goes (Downloads, a unique name;
//! Save … As: the system's save panel) and a pill at the bottom of the
//! pane while one runs and for a few seconds after, with Open (never for
//! installers and scripts) and Show in Finder. Nothing opens by itself.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use silo_web::{DownloadProgress, DownloadRequest, DownloadState};

use super::BrowserPane;
use crate::status::Status;

/// How long a finished download's pill stays.
const SHOWN: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Download {
    pub id: u32,
    pub name: String,
    pub path: PathBuf,
    pub received: u64,
    pub total: Option<u64>,
    pub state: DownloadState,
    /// Its last change.
    pub at: Instant,
}

fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} KB", b / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b / 1e6),
        _ => format!("{:.2} GB", b / 1e9),
    }
}

/// The pill for `list` at `now`: running downloads first, else the last
/// one finished in the last [`SHOWN`]; with the download it is about.
pub(crate) fn pill(list: &[Download], now: Instant) -> Option<(Status, &Download)> {
    let running: Vec<&Download> = list.iter().filter(|d| d.state == DownloadState::Running).collect();
    if let Some(last) = running.last() {
        let (received, total) = running.iter().fold((0, Some(0)), |(r, t), d| (r + d.received, t.zip(d.total).map(|(a, b)| a + b)));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let done = match total.filter(|&t| t > 0) {
            Some(total) => format!("{}%", (received as f64 / total as f64 * 100.0).floor().min(100.0) as u32),
            None => size(received),
        };
        let what = if running.len() > 1 { format!("{} files", running.len()) } else { last.name.clone() };
        return Some((Status::progress(format!("Downloading {what}… {done}")), last));
    }
    let last = list.iter().filter(|d| d.state != DownloadState::Cancelled && now.duration_since(d.at) < SHOWN).max_by_key(|d| d.at)?;
    let status = match last.state {
        DownloadState::Failed => Status::error(format!("Couldn't download {}", last.name)),
        _ => Status::success(format!("Downloaded {}", last.name)),
    };
    Some((status, last))
}

impl BrowserPane {
    /// A download needs its path: Downloads with a free name, or the save
    /// panel's choice for Save … As.
    pub(super) fn download_asked(&mut self, request: DownloadRequest, cx: &mut gpui::Context<Self>) {
        let Some(page) = self.page.clone() else { return };
        let dir = silo_web::downloads_dir();
        if !request.save_as {
            let path =
                silo_web::unique_path(&dir, &request.name, |p| self.downloads.iter().any(|d| d.state == DownloadState::Running && d.path == p));
            page.download_to(request.id, Some(&path));
            return self.download_started(request.id, path, cx);
        }
        let chosen = cx.prompt_for_new_path(&dir, Some(&request.name));
        cx.spawn(async move |this, cx| {
            let path = chosen.await.ok().and_then(Result::ok).flatten();
            let _ = this.update(cx, |this, cx| {
                page.download_to(request.id, path.as_deref());
                if let Some(path) = path {
                    this.download_started(request.id, path, cx);
                }
            });
        })
        .detach();
    }

    fn download_started(&mut self, id: u32, path: PathBuf, cx: &mut gpui::Context<Self>) {
        let name = path.file_name().map_or_else(|| "download".into(), |n| n.to_string_lossy().into_owned());
        self.downloads.retain(|d| d.state == DownloadState::Running || d.at.elapsed() < SHOWN);
        self.downloads.push(Download { id, name, path, received: 0, total: None, state: DownloadState::Running, at: Instant::now() });
        cx.notify();
    }

    pub(super) fn download_progress(&mut self, progress: DownloadProgress, cx: &mut gpui::Context<Self>) {
        let Some(download) = self.downloads.iter_mut().find(|d| d.id == progress.id) else { return };
        if let Some(path) = progress.path {
            download.name = path.file_name().map_or_else(|| download.name.clone(), |n| n.to_string_lossy().into_owned());
            download.path = path;
        }
        (download.received, download.total, download.at) = (progress.received, progress.total, Instant::now());
        if download.state == progress.state {
            return cx.notify();
        }
        download.state = progress.state;
        if progress.state != DownloadState::Running {
            tracing::info!(block = %self.block_id, state = ?progress.state, "download ended");
            // The pill goes once it has been shown long enough.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SHOWN).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
        cx.notify();
    }

    /// The pill over the bottom of the page, while there is one.
    pub(super) fn download_pill(&self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        use gpui::{div, prelude::*, px, rgb};
        let (status, download) = pill(&self.downloads, Instant::now())?;
        let (id, path, state) = (download.id, download.path.clone(), download.state);
        let button = |label: &'static str| {
            div()
                .id(label)
                .px(px(4.0))
                .cursor_pointer()
                .text_color(rgb(crate::theme::muted_on(crate::theme::overlay())))
                .hover(|s| s.text_color(rgb(crate::theme::ink_on(crate::theme::overlay()))))
                .child(label)
        };
        let mut card = crate::status::card(&status, cx).gap(px(7.0));
        match state {
            DownloadState::Running => {
                card = card.child(button("Cancel").on_click(cx.listener(move |this, _, _, _| {
                    if let Some(page) = &this.page {
                        page.cancel_download(id);
                    }
                })));
            }
            DownloadState::Done => {
                if !silo_web::dangerous(&download.name) {
                    let open = path.clone();
                    card = card.child(button("Open").on_click(move |_, _, cx| cx.open_with_system(&open)));
                }
                card = card.child(button("Show").on_click(move |_, _, cx| cx.reveal_path(&path)));
            }
            DownloadState::Failed | DownloadState::Cancelled => {}
        }
        let pill = div()
            .id("download-pill")
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(card);
        let bottom = crate::status::PILL_CLEARANCE;
        Some(div().absolute().bottom(px(bottom)).left_0().right_0().flex().justify_center().child(pill).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(id: u32, name: &str, received: u64, total: Option<u64>, state: DownloadState, at: Instant) -> Download {
        Download { id, name: name.into(), path: PathBuf::from(format!("/d/{name}")), received, total, state, at }
    }

    #[test]
    fn running_downloads_show_progress() {
        let now = Instant::now();
        let one = [download(1, "a.pdf", 420, Some(1000), DownloadState::Running, now)];
        assert_eq!(pill(&one, now).map(|(s, d)| (s.text, d.id)), Some(("Downloading a.pdf… 42%".into(), 1)));
        let sizeless = [download(1, "a.bin", 3_400_000, None, DownloadState::Running, now)];
        assert_eq!(pill(&sizeless, now).unwrap().0.text, "Downloading a.bin… 3.4 MB");
        let two = [download(1, "a", 100, Some(200), DownloadState::Running, now), download(2, "b", 100, Some(200), DownloadState::Running, now)];
        assert_eq!(pill(&two, now).unwrap().0.text, "Downloading 2 files… 50%");
    }

    #[test]
    fn finished_downloads_show_briefly() {
        let now = Instant::now();
        let done = [download(1, "a.pdf", 10, Some(10), DownloadState::Done, now)];
        let (status, _) = pill(&done, now).unwrap();
        assert_eq!((status.text.as_str(), status.kind), ("Downloaded a.pdf", crate::status::StatusKind::Success));
        assert!(pill(&done, now + SHOWN).is_none(), "gone after a while");
        let failed = [download(1, "a.pdf", 1, None, DownloadState::Failed, now)];
        assert_eq!(pill(&failed, now).unwrap().0.text, "Couldn't download a.pdf");
        let cancelled = [download(1, "a.pdf", 1, None, DownloadState::Cancelled, now)];
        assert!(pill(&cancelled, now).is_none(), "a cancel says nothing");
        let mixed = [done[0].clone(), download(2, "b.zip", 0, None, DownloadState::Running, now)];
        assert_eq!(pill(&mixed, now).unwrap().1.id, 2, "a running one wins");
    }
}
