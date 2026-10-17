use super::{NvimGpui, Session};
use crate::nvim::NvimProcess;
#[cfg(test)]
use crate::nvim::STARTUP_READY_TEST_TIMEOUT;
use gpui::{Context, Window};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const REMOTE_FILE_DROP_NOTICE: &str =
    "Remote mode does not support opening local files or directories.";
const FILE_DROP_NOTICE_DURATION: Duration = Duration::from_secs(4);

impl NvimGpui {
    pub(super) fn flush_pending_file_opens(&mut self, cx: &mut Context<Self>) {
        let requests = self.app.session.take_pending_file_opens();
        if requests.is_empty() {
            return;
        }
        cx.spawn(async move |_weak, _cx| {
            for request in requests {
                match request.recv().await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => log::error!(
                        target: "nvim_gpui::startup",
                        "Neovim could not open a queued platform file: {error}"
                    ),
                    Err(error) => log::warn!(
                        target: "nvim_gpui::startup",
                        "queued file-open request response was lost: {error}"
                    ),
                }
            }
        })
        .detach();
    }

    pub(super) fn start_open_urls_task(
        &mut self,
        open_urls: async_channel::Receiver<Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        self.window.open_urls_task = Some(cx.spawn(async move |weak, cx| {
            while let Ok(urls) = open_urls.recv().await {
                let paths = urls
                    .iter()
                    .filter_map(|url| path_from_open_url(url))
                    .collect::<Vec<_>>();
                if paths.is_empty() {
                    continue;
                }

                let requests =
                    match weak.update(cx, |this, _cx| this.app.session.queue_open_files(paths)) {
                        Ok(requests) => requests,
                        Err(_) => break,
                    };
                for request in requests {
                    match request.recv().await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            log::error!(
                                target: "nvim_gpui::startup",
                                "Neovim could not open a file from the platform: {error}"
                            );
                        }
                        Err(error) => {
                            log::warn!(
                                target: "nvim_gpui::startup",
                                "file-open request response was lost: {error}"
                            );
                        }
                    }
                }
            }
        }));
    }

    pub(super) fn on_file_drag_move(
        &mut self,
        _event: &gpui::DragMoveEvent<gpui::ExternalPaths>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .app
            .session
            .nvim
            .as_ref()
            .is_some_and(NvimProcess::is_remote)
            && self.window.file_drop_notice.is_none()
        {
            self.show_remote_file_drop_notice(cx);
        }
    }

    pub(super) fn on_file_drop(
        &mut self,
        paths: &gpui::ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.prevent_default();

        if self
            .app
            .session
            .nvim
            .as_ref()
            .is_some_and(NvimProcess::is_remote)
        {
            log::info!(
                target: "nvim_gpui::startup",
                "ignoring {} dropped path(s) in remote mode",
                paths.paths().len()
            );
            self.show_remote_file_drop_notice(cx);
            return;
        }

        let requests = self.app.session.queue_open_files(paths.paths().to_vec());
        if requests.is_empty() {
            return;
        }

        drop(cx.spawn(async move |_weak, _cx| {
            for request in requests {
                match request.recv().await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        log::error!(
                            target: "nvim_gpui::startup",
                            "Neovim could not open a dropped path: {error}"
                        );
                    }
                    Err(error) => {
                        log::warn!(
                            target: "nvim_gpui::startup",
                            "dropped-path open request response was lost: {error}"
                        );
                    }
                }
            }
        }));
    }

    fn show_remote_file_drop_notice(&mut self, cx: &mut Context<Self>) {
        self.window.file_drop_notice_generation =
            self.window.file_drop_notice_generation.wrapping_add(1);
        let generation = self.window.file_drop_notice_generation;
        self.window.file_drop_notice = Some(REMOTE_FILE_DROP_NOTICE.to_owned());
        self.window.file_drop_notice_task = Some(cx.spawn(async move |weak, cx| {
            gpui::Timer::after(FILE_DROP_NOTICE_DURATION).await;
            let _ = weak.update(cx, |this, cx| {
                if this.window.file_drop_notice_generation == generation {
                    this.window.file_drop_notice = None;
                    this.window.file_drop_notice_task = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

impl Session {
    pub(crate) fn queue_open_files(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> Vec<async_channel::Receiver<Result<rmpv::Value, String>>> {
        if self.nvim.is_none() || !self.nvim_startup_ready {
            log::warn!(
                target: "nvim_gpui::startup",
                "queueing {} platform file-open path(s) until Neovim startup completes",
                paths.len()
            );
            self.pending_file_opens.extend(paths);
            return Vec::new();
        }
        let nvim = self.nvim.as_ref().expect("Neovim startup is ready");
        if nvim.is_remote() {
            return Vec::new();
        }

        paths
            .into_iter()
            .filter_map(|path| {
                log::info!(
                    target: "nvim_gpui::startup",
                    "opening platform file {}",
                    path.display()
                );
                match nvim.edit_file(&path) {
                    Ok(request) => Some(request),
                    Err(error) => {
                        log::error!(
                            target: "nvim_gpui::startup",
                            "could not queue platform file {}: {error}",
                            path.display()
                        );
                        None
                    }
                }
            })
            .collect()
    }
}

fn path_from_open_url(raw_url: &str) -> Option<PathBuf> {
    if raw_url.is_empty() {
        return None;
    }
    if Path::new(raw_url).is_absolute() {
        return Some(PathBuf::from(raw_url));
    }

    let Ok(url) = url::Url::parse(raw_url) else {
        return Some(PathBuf::from(raw_url));
    };
    if url.scheme() != "file" {
        log::warn!(
            target: "nvim_gpui::startup",
            "ignoring unsupported platform open URL scheme: {}",
            url.scheme()
        );
        return None;
    }

    match url.to_file_path() {
        Ok(path) => Some(path),
        Err(()) => {
            log::warn!(
                target: "nvim_gpui::startup",
                "could not convert platform file URL to a path: {raw_url}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_file_requests_remain_queued_in_arrival_order() {
        let mut session = Session::default();
        let first = PathBuf::from("first.md");
        let second = PathBuf::from("second.md");
        assert!(session.queue_open_files(vec![first.clone()]).is_empty());
        assert!(session.queue_open_files(vec![second.clone()]).is_empty());
        assert!(session.take_pending_file_opens().is_empty());
        assert_eq!(session.pending_file_opens, vec![first, second]);
    }

    #[test]
    fn connected_nvim_does_not_open_platform_files_before_startup_ready() {
        let nvim = NvimProcess::spawn(
            80,
            24,
            ["-u", "NONE", "-i", "NONE", "-n"].map(std::ffi::OsString::from),
        )
        .unwrap();
        assert!(
            nvim.protocol().is_some(),
            "Neovim RPC protocol handshake did not complete during spawn (startup_theme_received={})",
            nvim.startup_theme().is_some()
        );
        let mut session = Session {
            nvim: Some(nvim),
            ..Session::default()
        };
        let files = vec![PathBuf::from("first.md"), PathBuf::from("second.md")];
        assert!(session.queue_open_files(files.clone()).is_empty());
        assert!(session.take_pending_file_opens().is_empty());
        assert_eq!(session.pending_file_opens, files);
        let events = session.nvim.as_ref().unwrap().events();
        let deadline = std::time::Instant::now() + STARTUP_READY_TEST_TIMEOUT;
        let mut saw_api_ready = false;
        let mut saw_ui_attached = false;
        let mut received_event_count = 0;
        loop {
            match events.try_recv() {
                Ok(crate::nvim::NvimEvent::StartupReady) => break,
                Ok(crate::nvim::NvimEvent::ApiReady { .. }) => {
                    saw_api_ready = true;
                    received_event_count += 1;
                }
                Ok(crate::nvim::NvimEvent::UiAttached { .. }) => {
                    saw_ui_attached = true;
                    received_event_count += 1;
                }
                Ok(crate::nvim::NvimEvent::Error(error)) => panic!(
                    "Neovim RPC failed before startup was ready (api_ready={saw_api_ready}, ui_attached={saw_ui_attached}): {error}"
                ),
                Ok(crate::nvim::NvimEvent::Disconnected { reason }) => panic!(
                    "Neovim disconnected before startup was ready (api_ready={saw_api_ready}, ui_attached={saw_ui_attached}): {reason:?}"
                ),
                Ok(_) => received_event_count += 1,
                Err(async_channel::TryRecvError::Closed) => {
                    panic!(
                        "Neovim event channel closed before startup was ready (api_ready={saw_api_ready}, ui_attached={saw_ui_attached}, event_count={received_event_count})"
                    );
                }
                Err(async_channel::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        let nvim = session.nvim.as_ref().unwrap();
                        let startup_state = nvim
                            .protocol()
                            .ok_or_else(|| "protocol metadata unavailable".to_owned())
                            .and_then(|protocol| {
                                nvim.request(
                                    "nvim_exec_lua",
                                    rmpv::Value::Array(vec![
                                        rmpv::Value::from(
                                            r#"local channel = ...
local ok, autocmds = pcall(vim.api.nvim_get_autocmds, { group = "nvim_gpui_startup_ready" })
return {
    vim_did_enter = vim.v.vim_did_enter,
    channel = channel,
    channel_info = vim.api.nvim_get_chan_info(channel),
    autocmd_query_ok = ok,
    autocmds = autocmds,
}"#,
                                        ),
                                        rmpv::Value::Array(vec![rmpv::Value::from(
                                            protocol.channel_id,
                                        )]),
                                    ]),
                                )
                                .map_err(|error| format!("could not queue startup-state query: {error}"))
                            })
                            .and_then(|response| {
                                response
                                    .recv_blocking()
                                    .map_err(|error| format!("startup-state query timed out: {error}"))?
                                    .map_err(|error| format!("startup-state query failed: {error}"))
                            });
                        panic!(
                            "startup was not ready within {:?} (nvim_version={:?}, startup_theme_received={}, api_ready={saw_api_ready}, ui_attached={saw_ui_attached}, event_count={received_event_count}, startup_state={startup_state:?})",
                            STARTUP_READY_TEST_TIMEOUT,
                            nvim.version(),
                            nvim.startup_theme().is_some()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
        session.nvim_startup_ready = true;
        let requests = session.take_pending_file_opens();
        assert_eq!(requests.len(), 2);
        assert!(session.pending_file_opens.is_empty());
        for request in requests {
            request.recv_blocking().unwrap().unwrap();
        }
        let current = session
            .nvim
            .as_ref()
            .unwrap()
            .request(
                "nvim_eval",
                rmpv::Value::Array(vec![rmpv::Value::from("expand('%:t')")]),
            )
            .unwrap()
            .recv_blocking()
            .unwrap()
            .unwrap();
        assert_eq!(current.as_str(), Some("second.md"));
        assert_eq!(
            session
                .queue_open_files(vec![PathBuf::from("third.md")])
                .len(),
            1
        );
        assert!(session.pending_file_opens.is_empty());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn file_open_urls_are_decoded_to_paths() {
        assert_eq!(
            path_from_open_url("file:///tmp/project%20notes.md"),
            Some(PathBuf::from("/tmp/project notes.md"))
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn file_open_urls_are_decoded_to_paths() {
        assert_eq!(
            path_from_open_url("file:///C:/project%20notes.md"),
            Some(PathBuf::from(r"C:\project notes.md"))
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn plain_file_open_paths_are_preserved() {
        assert_eq!(
            path_from_open_url("/tmp/project-notes.md"),
            Some(PathBuf::from("/tmp/project-notes.md"))
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plain_file_open_paths_are_preserved() {
        assert_eq!(
            path_from_open_url(r"C:\project-notes.md"),
            Some(PathBuf::from(r"C:\project-notes.md"))
        );
    }

    #[test]
    fn non_file_open_urls_are_ignored() {
        assert_eq!(path_from_open_url("https://example.com/file.md"), None);
    }
}
