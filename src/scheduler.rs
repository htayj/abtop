use crate::collector::process::{cmd_has_binary, get_process_info, is_descendant_of};
use crate::model::{AgentSession, RateLimitInfo, SessionStatus};
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

pub const CLAUDE_LIMIT_REACHED_PCT: f64 = 100.0;
pub const CLAUDE_RATE_LIMIT_MAX_AGE_SECS: u64 = 10 * 60;

#[derive(Debug, Clone, PartialEq)]
pub struct SchedulerSession {
    pub key: String,
    pub agent_cli: &'static str,
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub config_root: String,
    pub status: SessionStatus,
    pub autokill_enabled: bool,
}

impl SchedulerSession {
    pub fn from_agent_session(session: &AgentSession, autokill_enabled: bool) -> Self {
        Self {
            key: scheduler_session_key(session.agent_cli, &session.session_id, session.pid),
            agent_cli: session.agent_cli,
            pid: session.pid,
            session_id: session.session_id.clone(),
            cwd: session.cwd.clone(),
            config_root: session.config_root.clone(),
            status: session.status.clone(),
            autokill_enabled,
        }
    }

    pub fn is_interruptible(&self) -> bool {
        self.agent_cli == "claude" && self.autokill_enabled && self.status.is_active()
    }
}

pub fn scheduler_session_key(agent_cli: &str, session_id: &str, pid: u32) -> String {
    if session_id.is_empty() {
        format!("{agent_cli}:pid:{pid}")
    } else {
        format!("{agent_cli}:{session_id}")
    }
}

pub fn build_resume_command(session: &SchedulerSession) -> String {
    let claude_command = if session.session_id.is_empty() {
        "claude --continue".to_string()
    } else {
        format!("claude --resume {}", shell_quote(&session.session_id))
    };
    let command = if session.config_root.trim().is_empty() {
        claude_command
    } else {
        format!(
            "CLAUDE_CONFIG_DIR={} {}",
            shell_quote(&expand_home(&session.config_root)),
            claude_command
        )
    };

    if session.cwd.trim().is_empty() {
        command
    } else {
        format!("cd {} && {}", shell_quote(&session.cwd), command)
    }
}

fn expand_home(path: &str) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|home| home.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedSession {
    pub key: String,
    pub session_id: String,
    pub pid: u32,
    pub pane_target: String,
    pub reset_at: u64,
    pub resume_command: String,
    pub resume_sent: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SchedulerTickReport {
    pub interrupted: Vec<String>,
    pub resumed: Vec<String>,
    pub errors: Vec<String>,
}

impl SchedulerTickReport {
    pub fn has_activity(&self) -> bool {
        !self.interrupted.is_empty() || !self.resumed.is_empty() || !self.errors.is_empty()
    }
}

pub trait SchedulerExecutor {
    fn is_tmux_available(&mut self) -> bool;
    fn resolve_pane(&mut self, pid: u32) -> Result<String, String>;
    fn verify_claude_pid(&mut self, pid: u32) -> bool;
    fn kill_pid(&mut self, pid: u32) -> Result<(), String>;
    fn reset_and_resume(&mut self, pane_target: &str, resume_command: &str) -> Result<(), String>;
    fn send_continue(&mut self, pane_target: &str) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct ClaudeScheduler {
    limit_reset_at: Option<u64>,
    interrupted: HashMap<String, InterruptedSession>,
    last_completed_reset_at: Option<u64>,
}

impl ClaudeScheduler {
    pub fn tick<E: SchedulerExecutor>(
        &mut self,
        sessions: &[SchedulerSession],
        rate_limits: &[RateLimitInfo],
        now: u64,
        executor: &mut E,
    ) -> SchedulerTickReport {
        let mut report = SchedulerTickReport::default();

        if let Some(reset_at) = self.limit_reset_at {
            if now >= reset_at {
                self.resume_interrupted(executor, &mut report);
                if self.interrupted.is_empty() {
                    self.last_completed_reset_at = Some(reset_at);
                    self.limit_reset_at = None;
                }
                return report;
            } else {
                self.retry_pending_resumes(executor, &mut report);
            }
        }

        let Some(reset_at) = claude_five_hour_limit_reset_at(rate_limits, now) else {
            return report;
        };

        if let Some(active_reset_at) = self.limit_reset_at {
            if reset_at != active_reset_at {
                self.limit_reset_at = Some(reset_at);
                for interrupted in self.interrupted.values_mut() {
                    interrupted.reset_at = reset_at;
                }
            }
            return report;
        }

        if self.last_completed_reset_at == Some(reset_at) {
            return report;
        }

        self.limit_reset_at = Some(reset_at);
        if !executor.is_tmux_available() {
            report
                .errors
                .push("scheduler skipped: not running inside tmux".to_string());
            return report;
        }

        for session in sessions.iter().filter(|session| session.is_interruptible()) {
            self.interrupt_session(session, reset_at, executor, &mut report);
        }

        report
    }

    fn interrupt_session<E: SchedulerExecutor>(
        &mut self,
        session: &SchedulerSession,
        reset_at: u64,
        executor: &mut E,
        report: &mut SchedulerTickReport,
    ) {
        if self.interrupted.contains_key(&session.key) {
            return;
        }

        let pane_target = match executor.resolve_pane(session.pid) {
            Ok(pane_target) => pane_target,
            Err(err) => {
                report
                    .errors
                    .push(format!("scheduler skipped {}: {}", session.session_id, err));
                return;
            }
        };

        if !executor.verify_claude_pid(session.pid) {
            report.errors.push(format!(
                "scheduler skipped {}: PID {} is not Claude",
                session.session_id, session.pid
            ));
            return;
        }

        if let Err(err) = executor.kill_pid(session.pid) {
            report.errors.push(format!(
                "scheduler failed to kill {}: {}",
                session.session_id, err
            ));
            return;
        }

        let resume_command = build_resume_command(session);
        self.interrupted.insert(
            session.key.clone(),
            InterruptedSession {
                key: session.key.clone(),
                session_id: session.session_id.clone(),
                pid: session.pid,
                pane_target,
                reset_at,
                resume_command,
                resume_sent: false,
            },
        );
        report.interrupted.push(session.session_id.clone());
        self.retry_session_resume(&session.key, executor, report);
    }

    fn retry_pending_resumes<E: SchedulerExecutor>(
        &mut self,
        executor: &mut E,
        report: &mut SchedulerTickReport,
    ) {
        let keys = self
            .interrupted
            .iter()
            .filter(|(_, session)| !session.resume_sent)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.retry_session_resume(&key, executor, report);
        }
    }

    fn retry_session_resume<E: SchedulerExecutor>(
        &mut self,
        key: &str,
        executor: &mut E,
        report: &mut SchedulerTickReport,
    ) {
        let Some(session) = self.interrupted.get_mut(key) else {
            return;
        };
        if session.resume_sent {
            return;
        }
        match executor.reset_and_resume(&session.pane_target, &session.resume_command) {
            Ok(()) => session.resume_sent = true,
            Err(err) => report.errors.push(format!(
                "scheduler failed to reset/resume {}: {}",
                session.session_id, err
            )),
        }
    }

    fn resume_interrupted<E: SchedulerExecutor>(
        &mut self,
        executor: &mut E,
        report: &mut SchedulerTickReport,
    ) {
        let keys = self.interrupted.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            if self
                .interrupted
                .get(&key)
                .map(|session| !session.resume_sent)
                .unwrap_or(false)
            {
                self.retry_session_resume(&key, executor, report);
            }

            let Some(session) = self.interrupted.get(&key) else {
                continue;
            };
            if !session.resume_sent {
                continue;
            }
            let pane_target = session.pane_target.clone();
            let session_id = session.session_id.clone();
            match executor.send_continue(&pane_target) {
                Ok(()) => {
                    self.interrupted.remove(&key);
                    report.resumed.push(session_id);
                }
                Err(err) => report.errors.push(format!(
                    "scheduler failed to continue {}: {}",
                    session_id, err
                )),
            }
        }
    }

    pub fn interrupted_count(&self) -> usize {
        self.interrupted.len()
    }

    pub fn status(&self, now: u64) -> Option<String> {
        if self.interrupted.is_empty() {
            return None;
        }
        let reset_at = self.limit_reset_at?;
        let remaining = reset_at.saturating_sub(now);
        Some(format!(
            "scheduler: Claude 5h reset in {} ({} interrupted)",
            format_eta(remaining),
            self.interrupted.len()
        ))
    }
}

pub fn claude_five_hour_limit_reset_at(rate_limits: &[RateLimitInfo], now: u64) -> Option<u64> {
    rate_limits
        .iter()
        .filter(|rl| rl.source == "claude")
        .filter(|rl| rate_limit_is_fresh(rl, now))
        .filter(|rl| rl.five_hour_pct.unwrap_or(0.0) >= CLAUDE_LIMIT_REACHED_PCT)
        .filter_map(|rl| rl.five_hour_resets_at)
        .filter(|&reset_at| reset_at > now)
        .min()
}

fn rate_limit_is_fresh(rate_limit: &RateLimitInfo, now: u64) -> bool {
    match rate_limit.updated_at {
        Some(updated_at) if updated_at <= now => now - updated_at <= CLAUDE_RATE_LIMIT_MAX_AGE_SECS,
        _ => false,
    }
}

fn format_eta(seconds: u64) -> String {
    if seconds < 60 {
        format!("{}s", seconds)
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

#[derive(Debug, Default)]
pub struct TmuxSchedulerExecutor;

impl TmuxSchedulerExecutor {
    fn tmux_status(args: &[&str]) -> Result<(), String> {
        let status = Command::new("tmux")
            .args(args)
            .status()
            .map_err(|err| format!("tmux failed: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("tmux exited with status {status}"))
        }
    }

    fn tmux_send_line(pane_target: &str, line: &str) -> Result<(), String> {
        let status = Command::new("tmux")
            .args(["send-keys", "-t", pane_target, "-l", line])
            .status()
            .map_err(|err| format!("tmux send-keys failed: {err}"))?;
        if !status.success() {
            return Err(format!("tmux send-keys exited with status {status}"));
        }
        Self::tmux_status(&["send-keys", "-t", pane_target, "Enter"])
    }
}

impl SchedulerExecutor for TmuxSchedulerExecutor {
    fn is_tmux_available(&mut self) -> bool {
        std::env::var("TMUX").is_ok()
    }

    fn resolve_pane(&mut self, target_pid: u32) -> Result<String, String> {
        if !self.is_tmux_available() {
            return Err("not running inside tmux".to_string());
        }

        let output = Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{pane_pid} #{pane_id}"])
            .output()
            .map_err(|err| format!("tmux list-panes failed: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "tmux list-panes exited with status {}",
                output.status
            ));
        }

        let process_info = get_process_info();
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let mut parts = line.splitn(2, ' ');
            let Some(pane_pid) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
                continue;
            };
            let Some(pane_target) = parts.next() else {
                continue;
            };
            if target_pid == pane_pid || is_descendant_of(target_pid, pane_pid, &process_info) {
                return Ok(pane_target.to_string());
            }
        }

        Err("pane not found".to_string())
    }

    fn verify_claude_pid(&mut self, pid: u32) -> bool {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()
            .map(|output| {
                output.status.success()
                    && cmd_has_binary(String::from_utf8_lossy(&output.stdout).trim(), "claude")
            })
            .unwrap_or(false)
    }

    fn kill_pid(&mut self, pid: u32) -> Result<(), String> {
        let status = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .map_err(|err| format!("kill failed: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("kill exited with status {status}"))
        }
    }

    fn reset_and_resume(&mut self, pane_target: &str, resume_command: &str) -> Result<(), String> {
        std::thread::sleep(Duration::from_millis(200));
        Self::tmux_send_line(pane_target, "reset")?;
        std::thread::sleep(Duration::from_millis(200));
        Self::tmux_send_line(pane_target, resume_command)
    }

    fn send_continue(&mut self, pane_target: &str) -> Result<(), String> {
        Self::tmux_send_line(pane_target, "continue")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RateLimitInfo;
    use std::collections::{HashMap, HashSet};

    #[derive(Debug, Default)]
    struct MockExecutor {
        tmux: bool,
        panes: HashMap<u32, String>,
        verified: HashSet<u32>,
        reset_failures: HashMap<String, usize>,
        continue_failures: HashMap<String, usize>,
        actions: Vec<String>,
    }

    impl MockExecutor {
        fn with_tmux() -> Self {
            Self {
                tmux: true,
                ..Self::default()
            }
        }
    }

    impl SchedulerExecutor for MockExecutor {
        fn is_tmux_available(&mut self) -> bool {
            self.actions.push("tmux?".to_string());
            self.tmux
        }

        fn resolve_pane(&mut self, pid: u32) -> Result<String, String> {
            self.actions.push(format!("resolve:{pid}"));
            self.panes
                .get(&pid)
                .cloned()
                .ok_or_else(|| format!("pane not found for {pid}"))
        }

        fn verify_claude_pid(&mut self, pid: u32) -> bool {
            self.actions.push(format!("verify:{pid}"));
            self.verified.contains(&pid)
        }

        fn kill_pid(&mut self, pid: u32) -> Result<(), String> {
            self.actions.push(format!("kill:{pid}"));
            Ok(())
        }

        fn reset_and_resume(
            &mut self,
            pane_target: &str,
            resume_command: &str,
        ) -> Result<(), String> {
            self.actions
                .push(format!("reset_resume:{pane_target}:{resume_command}"));
            if let Some(remaining) = self.reset_failures.get_mut(pane_target) {
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err("reset failed".to_string());
                }
            }
            Ok(())
        }

        fn send_continue(&mut self, pane_target: &str) -> Result<(), String> {
            self.actions.push(format!("continue:{pane_target}"));
            if let Some(remaining) = self.continue_failures.get_mut(pane_target) {
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err("continue failed".to_string());
                }
            }
            Ok(())
        }
    }

    fn session(
        agent_cli: &'static str,
        pid: u32,
        session_id: &str,
        status: SessionStatus,
        autokill_enabled: bool,
    ) -> SchedulerSession {
        SchedulerSession {
            key: scheduler_session_key(agent_cli, session_id, pid),
            agent_cli,
            pid,
            session_id: session_id.to_string(),
            cwd: String::new(),
            config_root: String::new(),
            status,
            autokill_enabled,
        }
    }

    fn claude_limit(pct: f64, reset_at: u64, updated_at: u64) -> RateLimitInfo {
        RateLimitInfo {
            source: "claude".to_string(),
            five_hour_pct: Some(pct),
            five_hour_resets_at: Some(reset_at),
            seven_day_pct: None,
            seven_day_resets_at: None,
            updated_at: Some(updated_at),
        }
    }

    #[test]
    fn limit_event_interrupts_only_active_claude_sessions_with_autokill() {
        let now = 1_000;
        let reset_at = now + 3_600;
        let sessions = vec![
            session("claude", 11, "active-on", SessionStatus::Thinking, true),
            session("claude", 12, "active-off", SessionStatus::Executing, false),
            session("claude", 13, "waiting-on", SessionStatus::Waiting, true),
            session("codex", 14, "codex-on", SessionStatus::Thinking, true),
        ];
        let mut executor = MockExecutor::with_tmux();
        executor.panes.insert(11, "%1".to_string());
        executor.verified.insert(11);

        let mut scheduler = ClaudeScheduler::default();
        let report = scheduler.tick(
            &sessions,
            &[claude_limit(100.0, reset_at, now)],
            now,
            &mut executor,
        );

        assert_eq!(report.interrupted, vec!["active-on".to_string()]);
        assert_eq!(scheduler.interrupted_count(), 1);
        assert_eq!(
            executor.actions,
            vec![
                "tmux?",
                "resolve:11",
                "verify:11",
                "kill:11",
                "reset_resume:%1:claude --resume 'active-on'"
            ]
        );

        let report = scheduler.tick(
            &sessions,
            &[claude_limit(100.0, reset_at, now)],
            reset_at,
            &mut executor,
        );
        assert_eq!(report.resumed, vec!["active-on".to_string()]);
        assert!(executor.actions.contains(&"continue:%1".to_string()));
    }

    #[test]
    fn unsafe_sessions_are_not_killed_without_pane_and_pid_verification() {
        let now = 2_000;
        let reset_at = now + 60;
        let sessions = vec![
            session("claude", 21, "no-pane", SessionStatus::Executing, true),
            session("claude", 22, "bad-pid", SessionStatus::Thinking, true),
        ];
        let mut executor = MockExecutor::with_tmux();
        executor.panes.insert(22, "%2".to_string());

        let mut scheduler = ClaudeScheduler::default();
        let report = scheduler.tick(
            &sessions,
            &[claude_limit(101.0, reset_at, now)],
            now,
            &mut executor,
        );

        assert!(report.interrupted.is_empty());
        assert!(!executor.actions.iter().any(|a| a.starts_with("kill:")));
        assert_eq!(scheduler.interrupted_count(), 0);
    }

    #[test]
    fn same_limit_window_is_not_interrupted_twice() {
        let now = 3_000;
        let reset_at = now + 60;
        let sessions = vec![session(
            "claude",
            31,
            "active",
            SessionStatus::Executing,
            true,
        )];
        let mut executor = MockExecutor::with_tmux();
        executor.panes.insert(31, "%3".to_string());
        executor.verified.insert(31);
        let mut scheduler = ClaudeScheduler::default();

        let first = scheduler.tick(
            &sessions,
            &[claude_limit(100.0, reset_at, now)],
            now,
            &mut executor,
        );
        let second = scheduler.tick(
            &sessions,
            &[claude_limit(100.0, reset_at, now)],
            now + 10,
            &mut executor,
        );

        assert_eq!(first.interrupted, vec!["active".to_string()]);
        assert!(second.interrupted.is_empty());
        assert_eq!(
            executor.actions.iter().filter(|a| *a == "kill:31").count(),
            1
        );
    }

    #[test]
    fn killed_session_is_retained_when_reset_resume_fails_and_retried() {
        let now = 3_500;
        let reset_at = now + 60;
        let sessions = vec![session(
            "claude",
            35,
            "active",
            SessionStatus::Executing,
            true,
        )];
        let mut executor = MockExecutor::with_tmux();
        executor.panes.insert(35, "%35".to_string());
        executor.verified.insert(35);
        executor.reset_failures.insert("%35".to_string(), 1);
        let mut scheduler = ClaudeScheduler::default();

        let first = scheduler.tick(
            &sessions,
            &[claude_limit(100.0, reset_at, now)],
            now,
            &mut executor,
        );
        assert_eq!(first.interrupted, vec!["active".to_string()]);
        assert_eq!(first.errors.len(), 1);
        assert_eq!(scheduler.interrupted_count(), 1);

        let retry = scheduler.tick(
            &sessions,
            &[claude_limit(100.0, reset_at, now)],
            now + 1,
            &mut executor,
        );
        assert!(retry.errors.is_empty());
        assert_eq!(scheduler.interrupted_count(), 1);
        assert_eq!(
            executor
                .actions
                .iter()
                .filter(|a| a.starts_with("reset_resume:%35"))
                .count(),
            2
        );

        let resumed = scheduler.tick(&sessions, &[], reset_at, &mut executor);
        assert_eq!(resumed.resumed, vec!["active".to_string()]);
        assert_eq!(scheduler.interrupted_count(), 0);
    }

    #[test]
    fn failed_continue_is_retained_and_retried() {
        let now = 3_700;
        let reset_at = now + 60;
        let sessions = vec![session(
            "claude",
            37,
            "active",
            SessionStatus::Executing,
            true,
        )];
        let mut executor = MockExecutor::with_tmux();
        executor.panes.insert(37, "%37".to_string());
        executor.verified.insert(37);
        executor.continue_failures.insert("%37".to_string(), 1);
        let mut scheduler = ClaudeScheduler::default();

        scheduler.tick(
            &sessions,
            &[claude_limit(100.0, reset_at, now)],
            now,
            &mut executor,
        );
        let failed = scheduler.tick(&sessions, &[], reset_at, &mut executor);
        assert!(failed.resumed.is_empty());
        assert_eq!(failed.errors.len(), 1);
        assert_eq!(scheduler.interrupted_count(), 1);

        let retried = scheduler.tick(&sessions, &[], reset_at + 1, &mut executor);
        assert_eq!(retried.resumed, vec!["active".to_string()]);
        assert_eq!(scheduler.interrupted_count(), 0);
        assert_eq!(
            executor
                .actions
                .iter()
                .filter(|action| *action == "continue:%37")
                .count(),
            2
        );
    }

    #[test]
    fn ignores_limit_without_future_reset_timestamp() {
        let now = 4_000;
        let sessions = vec![session(
            "claude",
            41,
            "active",
            SessionStatus::Executing,
            true,
        )];
        let mut executor = MockExecutor::with_tmux();
        let mut scheduler = ClaudeScheduler::default();

        let report = scheduler.tick(
            &sessions,
            &[claude_limit(100.0, now, now)],
            now,
            &mut executor,
        );

        assert!(!report.has_activity());
        assert!(executor.actions.is_empty());
    }

    #[test]
    fn ignores_stale_limit_data() {
        let now = 5_000;
        let reset_at = now + 600;
        let sessions = vec![session(
            "claude",
            51,
            "active",
            SessionStatus::Executing,
            true,
        )];
        let mut executor = MockExecutor::with_tmux();
        let mut scheduler = ClaudeScheduler::default();

        let report = scheduler.tick(
            &sessions,
            &[claude_limit(
                100.0,
                reset_at,
                now - CLAUDE_RATE_LIMIT_MAX_AGE_SECS - 1,
            )],
            now,
            &mut executor,
        );

        assert!(!report.has_activity());
        assert!(executor.actions.is_empty());
    }

    #[test]
    fn resume_command_preserves_cwd_config_root_and_session_id() {
        let mut s = session("claude", 61, "abc'def", SessionStatus::Executing, true);
        s.cwd = "/tmp/project with spaces".to_string();
        s.config_root = "/tmp/claude config".to_string();

        assert_eq!(
            build_resume_command(&s),
            "cd '/tmp/project with spaces' && CLAUDE_CONFIG_DIR='/tmp/claude config' claude --resume 'abc'\\''def'"
        );
    }
}
