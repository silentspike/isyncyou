use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ring::rand::SecureRandom;

use isyncyou_core::{AccountConfig, Config};
pub use isyncyou_store::MobileJobKind;
use isyncyou_store::{
    mobile_backup_idempotency_key, mobile_restore_cloud_idempotency_key, MobileJob, MobileJobState,
    Store,
};

use crate::{agent_ops, BackupRun};

const MOBILE_BACKUP_SERVICES: &[&str] = &["mail", "calendar", "contacts", "todo", "onenote"];
const MOBILE_JOB_LEASE_TTL_SECS: i64 = 300;
const MOBILE_JOB_PLAN_LIMIT: usize = 64;
#[cfg(feature = "mobile-job-device-test-hooks")]
const MOBILE_JOB_DEVICE_TEST_HOOK_MAX_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MobileWorkerDeviceSnapshot {
    pub network_validated: bool,
    pub metered: bool,
    pub charging: bool,
    pub free_bytes: u64,
}

pub trait MobileJobExecutor: Send + Sync {
    fn run_backup(
        &self,
        cfg: &Config,
        account: &str,
        gate: &Arc<Mutex<()>>,
        services: &[String],
    ) -> Result<BackupRun, MobileJobExecutionError>;

    fn run_restore_cloud(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        gate: &Arc<Mutex<()>>,
    ) -> Result<String, MobileJobExecutionError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileJobRetryCode {
    Network,
    Timeout,
    Http408,
    Http425,
    RateLimited,
    Server,
}

impl MobileJobRetryCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Http408 => "http_408",
            Self::Http425 => "http_425",
            Self::RateLimited => "rate_limited",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileJobFailureCode {
    InvalidIntent,
    Unsupported,
    Policy,
    Authentication,
    Internal,
    RetryBudgetExhausted,
}

impl MobileJobFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidIntent => "invalid_intent",
            Self::Unsupported => "unsupported",
            Self::Policy => "policy",
            Self::Authentication => "authentication",
            Self::Internal => "internal",
            Self::RetryBudgetExhausted => "retry_budget_exhausted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileJobExecutionError {
    Retryable {
        code: MobileJobRetryCode,
        retry_after: Option<Duration>,
        redacted: String,
    },
    Terminal {
        code: MobileJobFailureCode,
        redacted: String,
    },
}

impl MobileJobExecutionError {
    fn terminal(code: MobileJobFailureCode, message: impl Into<String>) -> Self {
        Self::Terminal {
            code,
            redacted: crate::agent_ops::redact_agent_operation_text(&message.into()),
        }
    }

    fn from_refresh(error: isyncyou_engine::RefreshFailure) -> Self {
        use isyncyou_engine::RefreshFailureKind;
        match error.kind {
            RefreshFailureKind::Network => Self::Retryable {
                code: MobileJobRetryCode::Network,
                retry_after: None,
                redacted: error.redacted.to_string(),
            },
            RefreshFailureKind::Timeout => Self::Retryable {
                code: MobileJobRetryCode::Timeout,
                retry_after: None,
                redacted: error.redacted.to_string(),
            },
            RefreshFailureKind::Http(408) => Self::Retryable {
                code: MobileJobRetryCode::Http408,
                retry_after: None,
                redacted: error.redacted.to_string(),
            },
            RefreshFailureKind::Http(425) => Self::Retryable {
                code: MobileJobRetryCode::Http425,
                retry_after: None,
                redacted: error.redacted.to_string(),
            },
            RefreshFailureKind::Http(429) => Self::Retryable {
                code: MobileJobRetryCode::RateLimited,
                retry_after: None,
                redacted: error.redacted.to_string(),
            },
            RefreshFailureKind::Http(500..=599) => Self::Retryable {
                code: MobileJobRetryCode::Server,
                retry_after: None,
                redacted: error.redacted.to_string(),
            },
            RefreshFailureKind::Authentication => Self::terminal(
                MobileJobFailureCode::Authentication,
                "authentication required",
            ),
            RefreshFailureKind::Http(_) | RefreshFailureKind::Internal => {
                Self::terminal(MobileJobFailureCode::Internal, error.redacted)
            }
        }
    }

    fn from_restore(error: isyncyou_engine::RestoreError) -> Self {
        use isyncyou_engine::RestoreFailureKind;
        let redacted = "cloud restore failed".to_string();
        match error.kind {
            RestoreFailureKind::Network => Self::Retryable {
                code: MobileJobRetryCode::Network,
                retry_after: None,
                redacted,
            },
            RestoreFailureKind::Timeout => Self::Retryable {
                code: MobileJobRetryCode::Timeout,
                retry_after: None,
                redacted,
            },
            RestoreFailureKind::Http(408) => Self::Retryable {
                code: MobileJobRetryCode::Http408,
                retry_after: None,
                redacted,
            },
            RestoreFailureKind::Http(425) => Self::Retryable {
                code: MobileJobRetryCode::Http425,
                retry_after: None,
                redacted,
            },
            RestoreFailureKind::Http(429) => Self::Retryable {
                code: MobileJobRetryCode::RateLimited,
                retry_after: None,
                redacted,
            },
            RestoreFailureKind::Http(500..=599) => Self::Retryable {
                code: MobileJobRetryCode::Server,
                retry_after: None,
                redacted,
            },
            RestoreFailureKind::Authentication => Self::terminal(
                MobileJobFailureCode::Authentication,
                "authentication required",
            ),
            RestoreFailureKind::Invalid => Self::terminal(
                MobileJobFailureCode::InvalidIntent,
                "invalid restore request",
            ),
            RestoreFailureKind::Http(_) | RestoreFailureKind::Internal => {
                Self::terminal(MobileJobFailureCode::Internal, redacted)
            }
        }
    }

    #[cfg(test)]
    fn retryable(code: MobileJobRetryCode, retry_after: Option<Duration>) -> Self {
        Self::Retryable {
            redacted: format!("mobile job retry: {}", code.as_str()),
            code,
            retry_after,
        }
    }
}

struct LiveMobileJobExecutor;

impl MobileJobExecutor for LiveMobileJobExecutor {
    fn run_backup(
        &self,
        cfg: &Config,
        account: &str,
        gate: &Arc<Mutex<()>>,
        services: &[String],
    ) -> Result<BackupRun, MobileJobExecutionError> {
        crate::agent_ops::run_mobile_backup_account(cfg, account, gate, services).map_err(|error| {
            match error {
                crate::agent_ops::MobileBackupError::InvalidRequest => {
                    MobileJobExecutionError::terminal(
                        MobileJobFailureCode::InvalidIntent,
                        "invalid backup request",
                    )
                }
                crate::agent_ops::MobileBackupError::Authentication => {
                    MobileJobExecutionError::terminal(
                        MobileJobFailureCode::Authentication,
                        "authentication required",
                    )
                }
                crate::agent_ops::MobileBackupError::Refresh(error) => {
                    MobileJobExecutionError::from_refresh(error)
                }
            }
        })
    }

    fn run_restore_cloud(
        &self,
        cfg: &Config,
        account: &str,
        service: &str,
        id: &str,
        gate: &Arc<Mutex<()>>,
    ) -> Result<String, MobileJobExecutionError> {
        if !isyncyou_engine::cloud_restore_service_supported(service) {
            return Err(MobileJobExecutionError::terminal(
                MobileJobFailureCode::Unsupported,
                isyncyou_engine::unsupported_cloud_restore_service_error(service),
            ));
        }
        if !cfg.restore.cloud_restore_enabled {
            return Err(MobileJobExecutionError::terminal(
                MobileJobFailureCode::Policy,
                isyncyou_engine::cloud_restore_disabled_error(),
            ));
        }
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        let token =
            isyncyou_engine::auth::resolve_cached_restore_token(cfg, account).map_err(|_| {
                MobileJobExecutionError::terminal(
                    MobileJobFailureCode::Authentication,
                    "authentication required",
                )
            })?;
        isyncyou_engine::restore_cloud_classified(cfg, account, service, id, token)
            .map_err(MobileJobExecutionError::from_restore)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileJobRunOutcome {
    Succeeded {
        job_id: String,
        kind: MobileJobKind,
        summary: String,
    },
    Failed {
        job_id: String,
        kind: MobileJobKind,
        code: MobileJobFailureCode,
        error: String,
    },
    Retrying {
        job_id: String,
        kind: MobileJobKind,
        code: MobileJobRetryCode,
        retry_after_secs: Option<u64>,
    },
    Deferred {
        job_id: String,
        code: MobileJobDeferredCode,
    },
    Noop {
        job_id: String,
        code: MobileJobNoopCode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileJobDeferredCode {
    DeviceStateUnavailable,
    WifiOnly,
    ChargingOnly,
    InsufficientStorage,
    WorkerBusy,
    LeaseNotAcquired,
}

impl MobileJobDeferredCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeviceStateUnavailable => "device_state_unavailable",
            Self::WifiOnly => "wifi_only",
            Self::ChargingOnly => "charging_only",
            Self::InsufficientStorage => "insufficient_storage",
            Self::WorkerBusy => "worker_busy",
            Self::LeaseNotAcquired => "lease_not_acquired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileJobNoopCode {
    JobNoLongerRunning,
    JobStateChanged,
}

impl MobileJobNoopCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JobNoLongerRunning => "job_no_longer_running",
            Self::JobStateChanged => "job_state_changed",
        }
    }
}

#[derive(Clone)]
pub struct MobileJobRuntime {
    cfg: Config,
    gate: Arc<Mutex<()>>,
    events: Arc<isyncyou_webui::EventBus>,
    owner: String,
    seq: Arc<AtomicU64>,
    executor: Arc<dyn MobileJobExecutor>,
    execution_guard: Arc<Mutex<()>>,
    #[cfg(feature = "mobile-job-device-test-hooks")]
    device_test_hook_root: Arc<Mutex<Option<std::path::PathBuf>>>,
}

impl MobileJobRuntime {
    pub fn new(cfg: Config, gate: Arc<Mutex<()>>, events: Arc<isyncyou_webui::EventBus>) -> Self {
        let owner = process_owner_id();
        Self {
            cfg,
            gate,
            events,
            owner,
            seq: Arc::new(AtomicU64::new(0)),
            executor: Arc::new(LiveMobileJobExecutor),
            execution_guard: Arc::new(Mutex::new(())),
            #[cfg(feature = "mobile-job-device-test-hooks")]
            device_test_hook_root: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(feature = "mobile-job-device-test-hooks")]
    pub fn set_device_test_hook_root(&self, root: impl Into<std::path::PathBuf>) {
        *self
            .device_test_hook_root
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(root.into());
    }

    #[cfg(feature = "mobile-job-device-test-hooks")]
    fn device_test_hook(&self, phase: &str) {
        let root = self
            .device_test_hook_root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(root) = root else { return };
        let marker = root.join(format!("mobile-job-hook-{phase}"));
        if !marker.is_file() {
            return;
        }
        eprintln!("mobile job device-test hold: {phase}");
        std::thread::sleep(std::time::Duration::from_secs(
            MOBILE_JOB_DEVICE_TEST_HOOK_MAX_SECS,
        ));
    }

    #[cfg(feature = "mobile-job-device-test-hooks")]
    fn device_test_network_offline(&self) -> bool {
        self.device_test_hook_root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|root| root.join("mobile-job-network-offline").is_file())
    }

    #[cfg(test)]
    fn with_executor(
        cfg: Config,
        gate: Arc<Mutex<()>>,
        events: Arc<isyncyou_webui::EventBus>,
        executor: Arc<dyn MobileJobExecutor>,
    ) -> Self {
        let mut runtime = Self::new(cfg, gate, events);
        runtime.executor = executor;
        runtime.owner = "test-mobile-job-runtime".to_string();
        runtime
    }

    pub fn enqueue_backup(&self, account: &str, services: &[String]) -> Result<MobileJob, String> {
        let services = normalize_backup_services(services)?;
        let idempotency_key = mobile_backup_idempotency_key(account, &services)
            .map_err(|e| format!("backup idempotency key: {e}"))?;
        let intent_json = serde_json::json!({
            "op": "backup",
            "account": account,
            "services": services,
        })
        .to_string();
        let store = self.open_store(account)?;
        let job = store
            .create_mobile_job(
                &self.next_job_id(MobileJobKind::Backup),
                account,
                MobileJobKind::Backup,
                None,
                None,
                &idempotency_key,
                &intent_json,
                now_secs(),
            )
            .map_err(|e| format!("enqueue backup job: {e}"))?;
        self.events.notify();
        Ok(job)
    }

    pub fn enqueue_restore_cloud(
        &self,
        account: &str,
        service: &str,
        id: &str,
    ) -> Result<MobileJob, String> {
        if !isyncyou_engine::cloud_restore_service_supported(service) {
            return Err(isyncyou_engine::unsupported_cloud_restore_service_error(
                service,
            ));
        }
        if !self.cfg.restore.cloud_restore_enabled {
            return Err(isyncyou_engine::cloud_restore_disabled_error());
        }
        let idempotency_key = mobile_restore_cloud_idempotency_key(account, service, id)
            .map_err(|e| format!("restore-cloud idempotency key: {e}"))?;
        let intent_json = serde_json::json!({
            "op": "restore-cloud",
            "account": account,
            "service": service,
            "id": id,
        })
        .to_string();
        let store = self.open_store(account)?;
        let job = store
            .create_mobile_job(
                &self.next_job_id(MobileJobKind::RestoreCloud),
                account,
                MobileJobKind::RestoreCloud,
                Some(service),
                Some(id),
                &idempotency_key,
                &intent_json,
                now_secs(),
            )
            .map_err(|e| format!("enqueue restore-cloud job: {e}"))?;
        self.events.notify();
        Ok(job)
    }

    pub fn recover_and_run_available_jobs(
        &self,
        account: Option<&str>,
    ) -> Result<Vec<MobileJobRunOutcome>, String> {
        let accounts: Vec<String> = match account {
            Some(account) => vec![account.to_string()],
            None => self.cfg.accounts.iter().map(|a| a.id.clone()).collect(),
        };
        let mut job_ids = Vec::new();
        let now = now_secs();
        for account in accounts {
            let store = self.open_store(&account)?;
            store
                .reclaim_mobile_jobs_from_foreign_process(&self.owner, now)
                .map_err(|e| format!("reclaim mobile jobs for {account}: {e}"))?;
            job_ids.extend(
                store
                    .recoverable_mobile_jobs(&account, now)
                    .map_err(|e| format!("list recoverable mobile jobs for {account}: {e}"))?
                    .into_iter()
                    .map(|job| job.job_id),
            );
        }
        let mut outcomes = Vec::new();
        for job_id in job_ids {
            outcomes.push(self.run_one_job(&job_id)?);
        }
        Ok(outcomes)
    }

    pub fn mobile_worker_plan(
        &self,
        account: &str,
    ) -> Result<(Vec<(String, MobileJobKind)>, bool), String> {
        let store = self.open_store(account)?;
        let now = now_secs();
        store
            .reclaim_mobile_jobs_from_foreign_process(&self.owner, now)
            .map_err(|e| format!("reclaim mobile jobs for {account}: {e}"))?;
        let jobs = store
            .recoverable_mobile_jobs(account, now)
            .map_err(|e| format!("list mobile jobs for {account}: {e}"))?;
        let truncated = jobs.len() > MOBILE_JOB_PLAN_LIMIT;
        Ok((
            jobs.into_iter()
                .take(MOBILE_JOB_PLAN_LIMIT)
                .map(|job| (job.job_id, job.kind))
                .collect(),
            truncated,
        ))
    }

    pub fn mobile_worker_constraints(&self) -> (bool, bool, u64) {
        (
            self.cfg.sync.wifi_only,
            self.cfg.sync.charging_only,
            self.cfg.sync.min_free_bytes,
        )
    }

    pub fn run_mobile_job_for_worker(
        &self,
        job_id: &str,
        expected_kind: MobileJobKind,
        device: MobileWorkerDeviceSnapshot,
    ) -> Result<MobileJobRunOutcome, String> {
        if !device.network_validated {
            return Ok(MobileJobRunOutcome::Deferred {
                job_id: job_id.to_string(),
                code: MobileJobDeferredCode::DeviceStateUnavailable,
            });
        }
        if device.metered && self.cfg.sync.wifi_only {
            return Ok(MobileJobRunOutcome::Deferred {
                job_id: job_id.to_string(),
                code: MobileJobDeferredCode::WifiOnly,
            });
        }
        if !device.charging && self.cfg.sync.charging_only {
            return Ok(MobileJobRunOutcome::Deferred {
                job_id: job_id.to_string(),
                code: MobileJobDeferredCode::ChargingOnly,
            });
        }
        if device.free_bytes < self.cfg.sync.min_free_bytes {
            return Ok(MobileJobRunOutcome::Deferred {
                job_id: job_id.to_string(),
                code: MobileJobDeferredCode::InsufficientStorage,
            });
        }
        let Ok(_guard) = self.execution_guard.try_lock() else {
            return Ok(MobileJobRunOutcome::Deferred {
                job_id: job_id.to_string(),
                code: MobileJobDeferredCode::WorkerBusy,
            });
        };
        let (_, job) = self.find_job(job_id)?;
        if job.kind != expected_kind {
            return Err("job_kind_mismatch".to_string());
        }
        self.run_one_job(job_id)
    }

    pub fn run_one_job(&self, job_id: &str) -> Result<MobileJobRunOutcome, String> {
        let job = {
            let (store, job) = self.find_job(job_id)?;
            let acquired = store
                .acquire_mobile_job_lease(
                    job_id,
                    &self.owner,
                    now_secs(),
                    MOBILE_JOB_LEASE_TTL_SECS,
                )
                .map_err(|e| format!("lease mobile job {job_id}: {e}"))?;
            if !acquired {
                return Ok(MobileJobRunOutcome::Deferred {
                    job_id: job_id.to_string(),
                    code: MobileJobDeferredCode::LeaseNotAcquired,
                });
            }
            self.events.notify();
            store
                .get_mobile_job(job_id)
                .map_err(|e| format!("reload leased mobile job {job_id}: {e}"))?
                .unwrap_or(job)
        };

        #[cfg(feature = "mobile-job-device-test-hooks")]
        self.device_test_hook("after_lease");
        #[cfg(feature = "mobile-job-device-test-hooks")]
        let execution = if self.device_test_network_offline() {
            Err(MobileJobExecutionError::Retryable {
                code: MobileJobRetryCode::Network,
                retry_after: None,
                redacted: "mobile job network unavailable (device test hook)".to_string(),
            })
        } else {
            self.execute_job(&job)
        };
        #[cfg(not(feature = "mobile-job-device-test-hooks"))]
        let execution = self.execute_job(&job);
        #[cfg(feature = "mobile-job-device-test-hooks")]
        self.device_test_hook("after_execute_before_finish");
        let store = self.open_store(&job.account_id)?;
        match execution {
            Ok((result_json, summary)) => {
                let finished = store
                    .finish_mobile_job_if_running(
                        &job.job_id,
                        &self.owner,
                        now_secs(),
                        Some(&result_json),
                    )
                    .map_err(|e| format!("finish mobile job {}: {e}", job.job_id))?;
                if !finished {
                    self.events.notify();
                    return Ok(MobileJobRunOutcome::Noop {
                        job_id: job.job_id,
                        code: MobileJobNoopCode::JobNoLongerRunning,
                    });
                }
                self.record_job_activity(&store, &job, "succeeded", &summary)?;
                self.events.notify();
                Ok(MobileJobRunOutcome::Succeeded {
                    job_id: job.job_id,
                    kind: job.kind,
                    summary,
                })
            }
            Err(error) => match error {
                MobileJobExecutionError::Retryable {
                    code,
                    retry_after,
                    redacted,
                } if job.attempts < 5 => {
                    let progress = serde_json::json!({
                        "v": 1,
                        "status": "retry",
                        "code": code.as_str(),
                        "retry_after_secs": retry_after.map(|d| d.as_secs()),
                    })
                    .to_string();
                    let requeued = store
                        .requeue_mobile_job_if_running(
                            &job.job_id,
                            &self.owner,
                            &redacted,
                            &progress,
                            now_secs(),
                        )
                        .map_err(|e| format!("requeue mobile job {}: {e}", job.job_id))?;
                    if !requeued {
                        return Ok(MobileJobRunOutcome::Noop {
                            job_id: job.job_id,
                            code: MobileJobNoopCode::JobStateChanged,
                        });
                    }
                    self.events.notify();
                    Ok(MobileJobRunOutcome::Retrying {
                        job_id: job.job_id,
                        kind: job.kind,
                        code,
                        retry_after_secs: retry_after.map(|d| d.as_secs()),
                    })
                }
                MobileJobExecutionError::Retryable { .. }
                | MobileJobExecutionError::Terminal {
                    code: _,
                    redacted: _,
                } => {
                    let (code, error) = match error {
                        MobileJobExecutionError::Retryable { redacted, .. } => {
                            (MobileJobFailureCode::RetryBudgetExhausted, redacted)
                        }
                        MobileJobExecutionError::Terminal { code, redacted } => (code, redacted),
                    };
                    let error = format!("{}: {}", code.as_str(), error);
                    match store.fail_mobile_job_if_running(
                        &job.job_id,
                        &self.owner,
                        now_secs(),
                        &error,
                    ) {
                        Ok(true) => {
                            self.record_job_activity(&store, &job, "failed", &error)?;
                            self.events.notify();
                            Ok(MobileJobRunOutcome::Failed {
                                job_id: job.job_id,
                                kind: job.kind,
                                code,
                                error,
                            })
                        }
                        Ok(false) => {
                            self.events.notify();
                            Ok(MobileJobRunOutcome::Noop {
                                job_id: job.job_id,
                                code: MobileJobNoopCode::JobStateChanged,
                            })
                        }
                        Err(e) => Err(format!("fail mobile job {}: {e}", job.job_id)),
                    }
                }
            },
        }
    }

    fn execute_job(&self, job: &MobileJob) -> Result<(String, String), MobileJobExecutionError> {
        match job.kind {
            MobileJobKind::Backup => {
                let services = backup_services_from_intent(&job.intent_json).map_err(|e| {
                    MobileJobExecutionError::terminal(MobileJobFailureCode::InvalidIntent, e)
                })?;
                let run =
                    self.executor
                        .run_backup(&self.cfg, &job.account_id, &self.gate, &services)?;
                let result_json = serde_json::json!({
                    "summary": run.summary,
                    "delta": {
                        "mail": run.delta.mail,
                        "calendar": run.delta.calendar,
                        "contacts": run.delta.contacts,
                        "todo": run.delta.todo,
                        "onenote": run.delta.onenote,
                    }
                })
                .to_string();
                Ok((result_json, run.summary))
            }
            MobileJobKind::RestoreCloud => {
                let service = job.service.as_deref().ok_or_else(|| {
                    MobileJobExecutionError::terminal(
                        MobileJobFailureCode::InvalidIntent,
                        "restore-cloud job missing service",
                    )
                })?;
                let id = job.target_id.as_deref().ok_or_else(|| {
                    MobileJobExecutionError::terminal(
                        MobileJobFailureCode::InvalidIntent,
                        "restore-cloud job missing target_id",
                    )
                })?;
                let new_id = self.executor.run_restore_cloud(
                    &self.cfg,
                    &job.account_id,
                    service,
                    id,
                    &self.gate,
                )?;
                let safe_new_id = agent_ops::redact_agent_operation_text(&new_id);
                let result_json = serde_json::json!({
                    "service": service,
                    "restored": id,
                    "new_id": safe_new_id,
                })
                .to_string();
                Ok((
                    result_json,
                    format!("restore-cloud {service} queued item restored"),
                ))
            }
        }
    }

    fn record_job_activity(
        &self,
        store: &Store,
        job: &MobileJob,
        status: &str,
        summary: &str,
    ) -> Result<(), String> {
        let now = crate::unix_now();
        store
            .add_run(
                &job.account_id,
                &format!("mobile-job:{}", job.kind.as_str()),
                &now,
                &now,
                status,
                &agent_ops::redact_agent_operation_text(summary),
            )
            .map(|_| ())
            .map_err(|e| format!("record mobile job run {}: {e}", job.job_id))
    }

    fn find_job(&self, job_id: &str) -> Result<(Store, MobileJob), String> {
        for account in &self.cfg.accounts {
            let store = open_account_store(account)?;
            if let Some(job) = store
                .get_mobile_job(job_id)
                .map_err(|e| format!("lookup mobile job {job_id}: {e}"))?
            {
                return Ok((store, job));
            }
        }
        Err(format!("mobile job '{job_id}' not found"))
    }

    fn open_store(&self, account: &str) -> Result<Store, String> {
        let account = self
            .cfg
            .accounts
            .iter()
            .find(|a| a.id == account)
            .ok_or_else(|| format!("unknown account '{account}'"))?;
        open_account_store(account)
    }

    fn next_job_id(&self, kind: MobileJobKind) -> String {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        format!("mobile-{}-{}-{seq}", kind.as_str(), crate::unix_now_ms())
    }
}

fn process_owner_id() -> String {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER
        .get_or_init(|| {
            let mut nonce = [0u8; 16];
            ring::rand::SystemRandom::new()
                .fill(&mut nonce)
                .expect("system randomness required for mobile job owner");
            let nonce = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
            format!("mobile-process-{}-{nonce}", std::process::id())
        })
        .clone()
}

impl isyncyou_webui::BackupHandler for MobileJobRuntime {
    fn enqueue_backup(
        &self,
        account: &str,
        services: &[String],
    ) -> Result<isyncyou_webui::BackupJobQueued, String> {
        let job = MobileJobRuntime::enqueue_backup(self, account, services)?;
        Ok(isyncyou_webui::BackupJobQueued {
            job_id: job.job_id,
            state: job.state.as_str().to_string(),
        })
    }
}

impl isyncyou_webui::RestoreHandler for MobileJobRuntime {
    fn restore(
        &self,
        account: &str,
        service: &str,
        id: &str,
    ) -> Result<isyncyou_webui::RestoreResponse, String> {
        let job = MobileJobRuntime::enqueue_restore_cloud(self, account, service, id)?;
        Ok(isyncyou_webui::RestoreResponse::Queued {
            job_id: job.job_id,
            state: job.state.as_str().to_string(),
        })
    }
}

impl isyncyou_webui::MobileJobHandler for MobileJobRuntime {
    fn list_jobs(
        &self,
        account: &str,
        limit: u32,
    ) -> Result<Vec<isyncyou_webui::MobileJobSummary>, String> {
        let store = self.open_store(account)?;
        store
            .list_mobile_jobs(account, limit)
            .map_err(|e| format!("list mobile jobs: {e}"))
            .map(|jobs| {
                jobs.into_iter()
                    .map(|job| isyncyou_webui::MobileJobSummary {
                        job_id: job.job_id,
                        kind: job.kind.as_str().to_string(),
                        state: job.state.as_str().to_string(),
                        service: job.service,
                        target_id: job.target_id,
                        created_at: job.created_at,
                        updated_at: job.updated_at,
                        finished_at: job.finished_at,
                        last_error: job.last_error,
                    })
                    .collect()
            })
    }

    fn cancel_job(&self, account: &str, job_id: &str) -> Result<bool, String> {
        let store = self.open_store(account)?;
        let Some(job) = store
            .get_mobile_job(job_id)
            .map_err(|e| format!("get mobile job: {e}"))?
        else {
            return Ok(false);
        };
        if job.state.is_terminal() {
            return Ok(false);
        }
        match store.transition_mobile_job(
            job_id,
            MobileJobState::Cancelled,
            now_secs(),
            Some(r#"{"cancelled":true}"#),
            None,
            None,
        ) {
            Ok(()) => {
                self.events.notify();
                Ok(true)
            }
            Err(isyncyou_store::StoreError::IllegalMobileJobTransition(_)) => Ok(false),
            Err(e) => Err(format!("cancel mobile job: {e}")),
        }
    }
}

fn open_account_store(account: &AccountConfig) -> Result<Store, String> {
    std::fs::create_dir_all(&account.archive_root)
        .map_err(|e| format!("create archive root for {}: {e}", account.id))?;
    Store::open(account.archive_root.join(".isyncyou-store.db"))
        .map_err(|e| format!("open store for {}: {e}", account.id))
}

fn normalize_backup_services(services: &[String]) -> Result<Vec<String>, String> {
    let mut out = services.to_vec();
    out.sort();
    out.dedup();
    for service in &out {
        if !MOBILE_BACKUP_SERVICES.contains(&service.as_str()) {
            return Err(format!(
                "unsupported_backup_service: {}",
                agent_ops::redact_agent_operation_text(service)
            ));
        }
    }
    Ok(out)
}

fn backup_services_from_intent(intent_json: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(intent_json).map_err(|e| format!("parse backup job intent: {e}"))?;
    let Some(services) = v.get("services") else {
        return Ok(Vec::new());
    };
    let services = services
        .as_array()
        .ok_or_else(|| "backup job services must be an array".to_string())?
        .iter()
        .map(|v| {
            v.as_str()
                .map(ToString::to_string)
                .ok_or_else(|| "backup job service must be a string".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    normalize_backup_services(&services)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackupDelta, BackupRun};
    use isyncyou_core::AccountConfig;
    use std::path::PathBuf;
    use std::sync::Condvar;

    #[derive(Default)]
    struct RecordingExecutor {
        backup_calls: Mutex<Vec<Vec<String>>>,
        restore_calls: Mutex<Vec<(String, String)>>,
        fail_with: Mutex<Option<MobileJobExecutionError>>,
    }

    impl RecordingExecutor {
        fn fail(error: &str) -> Self {
            Self {
                fail_with: Mutex::new(Some(MobileJobExecutionError::terminal(
                    MobileJobFailureCode::Internal,
                    error,
                ))),
                ..Self::default()
            }
        }

        fn retry(code: MobileJobRetryCode) -> Self {
            Self {
                fail_with: Mutex::new(Some(MobileJobExecutionError::retryable(
                    code,
                    Some(Duration::from_secs(7)),
                ))),
                ..Self::default()
            }
        }

        fn backup_calls(&self) -> usize {
            self.backup_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len()
        }

        fn restore_calls(&self) -> usize {
            self.restore_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len()
        }
    }

    impl MobileJobExecutor for RecordingExecutor {
        fn run_backup(
            &self,
            _cfg: &Config,
            _account: &str,
            _gate: &Arc<Mutex<()>>,
            services: &[String],
        ) -> Result<BackupRun, MobileJobExecutionError> {
            if let Some(error) = self
                .fail_with
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return Err(error);
            }
            self.backup_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(services.to_vec());
            Ok(BackupRun {
                summary: "backup ok".to_string(),
                delta: BackupDelta {
                    mail: 1,
                    ..Default::default()
                },
            })
        }

        fn run_restore_cloud(
            &self,
            _cfg: &Config,
            _account: &str,
            service: &str,
            id: &str,
            _gate: &Arc<Mutex<()>>,
        ) -> Result<String, MobileJobExecutionError> {
            if let Some(error) = self
                .fail_with
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return Err(error);
            }
            self.restore_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((service.to_string(), id.to_string()));
            Ok(format!("new-{id}"))
        }
    }

    #[derive(Default)]
    struct BlockingExecutor {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    impl BlockingExecutor {
        fn wait_until_entered(&self) {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(5), |(entered, _)| !*entered)
                .unwrap_or_else(|e| e.into_inner());
            assert!(!timeout.timed_out(), "first worker did not enter executor");
            assert!(state.0);
        }

        fn release(&self) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.1 = true;
            self.changed.notify_all();
        }
    }

    impl MobileJobExecutor for BlockingExecutor {
        fn run_backup(
            &self,
            _cfg: &Config,
            _account: &str,
            _gate: &Arc<Mutex<()>>,
            _services: &[String],
        ) -> Result<BackupRun, MobileJobExecutionError> {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            Ok(BackupRun {
                summary: "backup ok".to_string(),
                delta: BackupDelta::default(),
            })
        }

        fn run_restore_cloud(
            &self,
            _cfg: &Config,
            _account: &str,
            _service: &str,
            _id: &str,
            _gate: &Arc<Mutex<()>>,
        ) -> Result<String, MobileJobExecutionError> {
            Err(MobileJobExecutionError::terminal(
                MobileJobFailureCode::Unsupported,
                "restore not used by concurrency test",
            ))
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "isyncyou-mobile-jobs-{name}-{}-{}",
            std::process::id(),
            crate::unix_now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_cfg(name: &str) -> Config {
        let root = temp_root(name);
        let archive = root.join("archive");
        let sync = root.join("sync");
        let cache = root.join("cache");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        Config {
            accounts: vec![AccountConfig {
                id: "me".to_string(),
                username: "me".to_string(),
                sync_root: sync,
                archive_root: archive,
                cache_root: cache,
                mount_point: None,
            }],
            restore: isyncyou_core::config::RestoreConfig {
                cloud_restore_enabled: true,
            },
            ..Default::default()
        }
    }

    fn runtime_with_executor(cfg: Config, executor: Arc<RecordingExecutor>) -> MobileJobRuntime {
        MobileJobRuntime::with_executor(
            cfg,
            Arc::new(Mutex::new(())),
            Arc::new(isyncyou_webui::EventBus::new()),
            executor,
        )
    }

    #[test]
    fn mobile_backup_job_enqueue_returns_existing_open_job() {
        let executor = Arc::new(RecordingExecutor::default());
        let rt = runtime_with_executor(test_cfg("backup-dedupe"), executor.clone());
        let services = vec!["mail".to_string(), "calendar".to_string()];
        let first = rt.enqueue_backup("me", &services).unwrap();
        let second = rt
            .enqueue_backup("me", &["calendar".to_string(), "mail".to_string()])
            .unwrap();
        assert_eq!(first.job_id, second.job_id);
        assert_eq!(executor.backup_calls(), 0);
    }

    #[test]
    fn mobile_backup_job_enqueue_only_worker_executes_later() {
        let executor = Arc::new(RecordingExecutor::default());
        let rt = runtime_with_executor(test_cfg("backup-worker"), executor.clone());
        let job = rt.enqueue_backup("me", &["mail".to_string()]).unwrap();
        assert_eq!(executor.backup_calls(), 0);

        let out = rt.run_one_job(&job.job_id).unwrap();
        assert!(matches!(
            out,
            MobileJobRunOutcome::Succeeded {
                kind: MobileJobKind::Backup,
                ..
            }
        ));
        assert_eq!(executor.backup_calls(), 1);
        let store = rt.open_store("me").unwrap();
        let job = store.get_mobile_job(&job.job_id).unwrap().unwrap();
        assert_eq!(job.state, MobileJobState::Succeeded);
        assert!(job.result_json.unwrap().contains("backup ok"));
        let runs = store.recent_runs("me", 10).unwrap();
        assert!(runs
            .iter()
            .any(|run| run.kind == "mobile-job:backup" && run.status == "succeeded"));
    }

    #[test]
    fn concurrent_mobile_workers_defer_second_as_worker_busy() {
        let executor = Arc::new(BlockingExecutor::default());
        let runtime = Arc::new(MobileJobRuntime::with_executor(
            test_cfg("worker-busy"),
            Arc::new(Mutex::new(())),
            Arc::new(isyncyou_webui::EventBus::new()),
            executor.clone(),
        ));
        let job = runtime.enqueue_backup("me", &["mail".to_string()]).unwrap();
        let first_runtime = runtime.clone();
        let first_job_id = job.job_id.clone();
        let first = std::thread::spawn(move || {
            first_runtime.run_mobile_job_for_worker(
                &first_job_id,
                MobileJobKind::Backup,
                MobileWorkerDeviceSnapshot {
                    network_validated: true,
                    metered: false,
                    charging: true,
                    free_bytes: u64::MAX,
                },
            )
        });
        executor.wait_until_entered();

        let second = runtime
            .run_mobile_job_for_worker(
                &job.job_id,
                MobileJobKind::Backup,
                MobileWorkerDeviceSnapshot {
                    network_validated: true,
                    metered: false,
                    charging: true,
                    free_bytes: u64::MAX,
                },
            )
            .unwrap();
        assert!(matches!(
            second,
            MobileJobRunOutcome::Deferred {
                code: MobileJobDeferredCode::WorkerBusy,
                ..
            }
        ));

        executor.release();
        assert!(matches!(
            first.join().unwrap().unwrap(),
            MobileJobRunOutcome::Succeeded { .. }
        ));
    }

    #[test]
    fn mobile_restore_cloud_job_worker_calls_existing_restore_path() {
        let executor = Arc::new(RecordingExecutor::default());
        let rt = runtime_with_executor(test_cfg("restore-worker"), executor.clone());
        let job = rt.enqueue_restore_cloud("me", "mail", "source-1").unwrap();
        assert_eq!(executor.restore_calls(), 0);

        let out = rt.run_one_job(&job.job_id).unwrap();
        assert!(matches!(
            out,
            MobileJobRunOutcome::Succeeded {
                kind: MobileJobKind::RestoreCloud,
                ..
            }
        ));
        assert_eq!(executor.restore_calls(), 1);
        let calls = executor
            .restore_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls[0], ("mail".to_string(), "source-1".to_string()));
    }

    #[test]
    fn mobile_restore_cloud_job_recovery_after_running_lease_expiry_reconciles() {
        let executor = Arc::new(RecordingExecutor::default());
        let rt = runtime_with_executor(test_cfg("restore-recover"), executor.clone());
        let job = rt.enqueue_restore_cloud("me", "mail", "source-1").unwrap();
        {
            let store = rt.open_store("me").unwrap();
            assert!(store
                .acquire_mobile_job_lease(&job.job_id, "dead-worker", now_secs() - 600, 10)
                .unwrap());
        }

        let out = rt.recover_and_run_available_jobs(Some("me")).unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            MobileJobRunOutcome::Succeeded {
                kind: MobileJobKind::RestoreCloud,
                ..
            }
        ));
        assert_eq!(executor.restore_calls(), 1);
    }

    #[test]
    fn mobile_job_worker_does_not_duplicate_restore_after_restart() {
        let executor = Arc::new(RecordingExecutor::default());
        let rt = runtime_with_executor(test_cfg("restore-no-dupe"), executor.clone());
        let job = rt.enqueue_restore_cloud("me", "mail", "source-1").unwrap();
        rt.run_one_job(&job.job_id).unwrap();
        assert_eq!(executor.restore_calls(), 1);

        let out = rt.recover_and_run_available_jobs(Some("me")).unwrap();
        assert!(out.is_empty());
        assert_eq!(executor.restore_calls(), 1);
    }

    #[test]
    fn mobile_job_errors_are_redacted() {
        let executor = Arc::new(RecordingExecutor::fail(
            "restore failed at https://tenant.example/cb?code=secret for owner@example.com refresh_token=abc",
        ));
        let rt = runtime_with_executor(test_cfg("error-redaction"), executor);
        let job = rt.enqueue_restore_cloud("me", "mail", "source-1").unwrap();
        let out = rt.run_one_job(&job.job_id).unwrap();
        assert!(matches!(out, MobileJobRunOutcome::Failed { .. }));

        let store = rt.open_store("me").unwrap();
        let job = store.get_mobile_job(&job.job_id).unwrap().unwrap();
        let err = job.last_error.unwrap();
        assert!(!err.contains("https://tenant.example"));
        assert!(!err.contains("owner@example.com"));
        assert!(!err.contains("refresh_token"));
        assert_eq!(job.state, MobileJobState::Failed);
    }

    #[test]
    fn mobile_job_retryable_error_requeues_and_clears_owner_lease() {
        let executor = Arc::new(RecordingExecutor::retry(MobileJobRetryCode::RateLimited));
        let rt = runtime_with_executor(test_cfg("retry-requeue"), executor);
        let job = rt.enqueue_backup("me", &["mail".to_string()]).unwrap();

        let out = rt.run_one_job(&job.job_id).unwrap();
        assert!(matches!(
            out,
            MobileJobRunOutcome::Retrying {
                code: MobileJobRetryCode::RateLimited,
                retry_after_secs: Some(7),
                ..
            }
        ));
        let stored = rt
            .open_store("me")
            .unwrap()
            .get_mobile_job(&job.job_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, MobileJobState::Queued);
        assert_eq!(stored.lease_owner, None);
        assert!(stored.progress_json.unwrap().contains("rate_limited"));
    }

    #[test]
    fn mobile_job_terminal_error_is_failed_without_text_classification() {
        let executor = Arc::new(RecordingExecutor::fail("temporarily unavailable 503"));
        let rt = runtime_with_executor(test_cfg("terminal-no-text-classification"), executor);
        let job = rt.enqueue_backup("me", &["mail".to_string()]).unwrap();

        let out = rt.run_one_job(&job.job_id).unwrap();
        assert!(matches!(
            out,
            MobileJobRunOutcome::Failed {
                error,
                ..
            } if error.starts_with("internal:")
        ));
        let stored = rt
            .open_store("me")
            .unwrap()
            .get_mobile_job(&job.job_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, MobileJobState::Failed);
    }

    #[test]
    fn live_backup_refresh_errors_map_structurally_without_text_matching() {
        let cases = [
            (
                isyncyou_engine::RefreshFailureKind::Network,
                MobileJobRetryCode::Network,
            ),
            (
                isyncyou_engine::RefreshFailureKind::Timeout,
                MobileJobRetryCode::Timeout,
            ),
            (
                isyncyou_engine::RefreshFailureKind::Http(408),
                MobileJobRetryCode::Http408,
            ),
            (
                isyncyou_engine::RefreshFailureKind::Http(425),
                MobileJobRetryCode::Http425,
            ),
            (
                isyncyou_engine::RefreshFailureKind::Http(429),
                MobileJobRetryCode::RateLimited,
            ),
            (
                isyncyou_engine::RefreshFailureKind::Http(503),
                MobileJobRetryCode::Server,
            ),
        ];
        for (kind, expected) in cases {
            let mapped = MobileJobExecutionError::from_refresh(isyncyou_engine::RefreshFailure {
                kind,
                redacted: "unrelated text",
            });
            assert!(matches!(
                mapped,
                MobileJobExecutionError::Retryable { code, .. } if code == expected
            ));
        }
        assert!(matches!(
            MobileJobExecutionError::from_refresh(isyncyou_engine::RefreshFailure {
                kind: isyncyou_engine::RefreshFailureKind::Internal,
                redacted: "network timeout HTTP 503",
            }),
            MobileJobExecutionError::Terminal {
                code: MobileJobFailureCode::Internal,
                ..
            }
        ));
    }

    #[test]
    fn live_restore_errors_map_structurally_without_text_matching() {
        use isyncyou_graph::http::{GraphTransportFailure, UploadError};

        let graph_cases = [
            (
                UploadError::Transport {
                    failure: GraphTransportFailure::Other,
                    detail: "unrelated text".into(),
                },
                MobileJobRetryCode::Network,
            ),
            (
                UploadError::Timeout("unrelated text".into()),
                MobileJobRetryCode::Timeout,
            ),
            (
                UploadError::Http {
                    status: 408,
                    body: "unrelated text".into(),
                },
                MobileJobRetryCode::Http408,
            ),
            (
                UploadError::Http {
                    status: 425,
                    body: "unrelated text".into(),
                },
                MobileJobRetryCode::Http425,
            ),
            (
                UploadError::Http {
                    status: 429,
                    body: "unrelated text".into(),
                },
                MobileJobRetryCode::RateLimited,
            ),
            (
                UploadError::Http {
                    status: 503,
                    body: "unrelated text".into(),
                },
                MobileJobRetryCode::Server,
            ),
        ];
        for (graph_error, expected) in graph_cases {
            let mapped = MobileJobExecutionError::from_restore(
                isyncyou_engine::RestoreError::from_graph(graph_error),
            );
            assert!(matches!(
                mapped,
                MobileJobExecutionError::Retryable { code, .. } if code == expected
            ));
        }

        assert!(matches!(
            MobileJobExecutionError::from_restore(isyncyou_engine::RestoreError::from_graph(
                UploadError::Http {
                    status: 401,
                    body: "network timeout HTTP 503".into(),
                },
            )),
            MobileJobExecutionError::Terminal {
                code: MobileJobFailureCode::Authentication,
                ..
            }
        ));
        assert!(matches!(
            MobileJobExecutionError::from_restore(isyncyou_engine::RestoreError::internal(
                "network timeout HTTP 503",
            )),
            MobileJobExecutionError::Terminal {
                code: MobileJobFailureCode::Internal,
                ..
            }
        ));
        assert!(matches!(
            MobileJobExecutionError::from_restore(isyncyou_engine::RestoreError::invalid(
                "invalid request",
            )),
            MobileJobExecutionError::Terminal {
                code: MobileJobFailureCode::InvalidIntent,
                ..
            }
        ));
    }

    #[test]
    fn mobile_backup_completed_job_does_not_block_future_backup_job() {
        let executor = Arc::new(RecordingExecutor::default());
        let rt = runtime_with_executor(test_cfg("backup-terminal-retry"), executor);
        let services = vec!["mail".to_string()];
        let first = rt.enqueue_backup("me", &services).unwrap();
        rt.run_one_job(&first.job_id).unwrap();
        let second = rt.enqueue_backup("me", &services).unwrap();
        assert_ne!(first.job_id, second.job_id);
    }

    #[cfg(feature = "mobile-job-device-test-hooks")]
    #[test]
    fn mobile_job_test_hook_is_bounded() {
        assert_eq!(MOBILE_JOB_DEVICE_TEST_HOOK_MAX_SECS, 120);
    }

    #[cfg(feature = "mobile-job-device-test-hooks")]
    #[test]
    fn mobile_job_network_test_hook_requeues_without_calling_executor() {
        let executor = Arc::new(RecordingExecutor::default());
        let rt = runtime_with_executor(test_cfg("network-hook"), executor.clone());
        let hook_root = temp_root("network-hook-marker");
        rt.set_device_test_hook_root(&hook_root);
        std::fs::write(hook_root.join("mobile-job-network-offline"), b"test").unwrap();

        let job = rt.enqueue_backup("me", &["mail".to_string()]).unwrap();
        let outcome = rt.run_one_job(&job.job_id).unwrap();
        assert!(matches!(
            outcome,
            MobileJobRunOutcome::Retrying {
                code: MobileJobRetryCode::Network,
                ..
            }
        ));
        assert_eq!(executor.backup_calls(), 0);

        std::fs::remove_file(hook_root.join("mobile-job-network-offline")).unwrap();
        let outcome = rt.run_one_job(&job.job_id).unwrap();
        assert!(matches!(
            outcome,
            MobileJobRunOutcome::Succeeded {
                kind: MobileJobKind::Backup,
                ..
            }
        ));
        assert_eq!(executor.backup_calls(), 1);
    }

    #[cfg(not(feature = "mobile-job-device-test-hooks"))]
    #[test]
    fn mobile_job_test_hook_does_not_change_default_build() {
        // The test itself is compiled only in the product's default feature set;
        // the hook code is absent from that build by construction.
    }
}
