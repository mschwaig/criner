use crate::persistence::{CrateVersionTable, TableAccess};
use crate::{
    error::Result,
    model, persistence,
    persistence::{new_key_insertion, ReportsTree},
};
use async_trait::async_trait;
use rusqlite::{params, TransactionBehavior};
use std::path::{Path, PathBuf};

fn all_but_recently_yanked(
    crate_name: &str,
    versions: &[String],
    table: &CrateVersionTable,
    key_buf: &mut String,
) -> Result<usize> {
    let mut num_yanked = 0;
    for version in versions.iter().rev() {
        key_buf.clear();
        model::CrateVersion::key_from(crate_name, version, key_buf);

        let is_yanked = table
            .get(&key_buf)?
            .map(|v| v.kind == crate::model::ChangeKind::Yanked)
            .unwrap_or(true);
        if is_yanked {
            num_yanked += 1;
        } else {
            break;
        }
    }
    Ok(versions.len() - num_yanked)
}

#[derive(Clone)]
pub struct WriteRequest {
    pub path: PathBuf,
    pub content: Vec<u8>,
}

#[derive(Clone)]
pub enum WriteInstruction {
    Skip,
    DoWrite(WriteRequest),
}

pub type WriteCallbackState = Option<async_channel::Sender<WriteRequest>>;
pub type WriteCallback =
    fn(WriteRequest, &WriteCallbackState) -> futures_util::future::BoxFuture<Result<WriteInstruction>>;

#[async_trait]
pub trait Aggregate
where
    Self: Sized,
{
    fn merge(self, other: Self) -> Self;
    async fn complete(&mut self, progress: &mut prodash::tree::Item, out: &mut Vec<u8>) -> Result<()>;
    async fn load_previous_state(&self, out_dir: &Path, progress: &mut prodash::tree::Item) -> Option<Self>;
    async fn load_previous_top_level_state(out_dir: &Path, progress: &mut prodash::tree::Item) -> Option<Self>;
    async fn store_current_state(&self, out_dir: &Path, progress: &mut prodash::tree::Item) -> Result<()>;
}

#[async_trait]
pub trait Generator {
    type Report: Aggregate + Send + Sync + Clone;
    type DBResult: Send;

    fn name() -> &'static str;
    fn version() -> &'static str;

    fn fq_result_key(crate_name: &str, crate_version: &str, key_buf: &mut String);
    fn fq_report_key(crate_name: &str, crate_version: &str, key_buf: &mut String) {
        ReportsTree::key_buf(crate_name, crate_version, Self::name(), Self::version(), key_buf);
    }

    fn get_result(
        connection: persistence::ThreadSafeConnection,
        crate_name: &str,
        crate_version: &str,
        key_buf: &mut String,
    ) -> Result<Option<Self::DBResult>>;

    async fn merge_reports(
        out_dir: PathBuf,
        cache_dir: Option<PathBuf>,
        mut progress: prodash::tree::Item,
        reports: async_channel::Receiver<Result<Option<Self::Report>>>,
        write: WriteCallback,
        write_state: WriteCallbackState,
    ) -> Result<()> {
        let mut report = None::<Self::Report>;
        let mut count = 0;
        while let Ok(result) = reports.recv().await {
            count += 1;
            progress.set(count);
            match result {
                Ok(Some(new_report)) => {
                    report = Some(match report {
                        Some(report) => report.merge(new_report),
                        None => new_report,
                    })
                }
                Ok(None) => {}
                Err(err) => {
                    progress.fail(format!("report failed: {}", err));
                }
            };
        }
        if let Some(mut report) = report {
            let previous_report = match cache_dir.as_ref() {
                Some(cd) => match Self::Report::load_previous_top_level_state(cd, &mut progress).await {
                    Some(r) => Some(r),
                    None => report.load_previous_state(cd, &mut progress).await,
                },
                None => None,
            };
            report = match previous_report {
                Some(previous_report) => previous_report.merge(report),
                None => report,
            };
            {
                complete_and_write_report(
                    &mut report,
                    Vec::new(),
                    &mut progress,
                    out_dir.join("index.html"),
                    write,
                    &write_state,
                )
                .await?;
            }
            if let Some(cd) = cache_dir {
                report.store_current_state(&cd, &mut progress).await?;
            }
        }
        Ok(())
    }

    async fn generate_report(
        crate_name: &str,
        crate_version: &str,
        result: Self::DBResult,
        progress: &mut prodash::tree::Item,
    ) -> Result<Self::Report>;

    async fn write_files(
        db: persistence::Db,
        out_dir: PathBuf,
        cache_dir: Option<PathBuf>,
        krates: Vec<(String, Vec<u8>)>,
        mut progress: prodash::tree::Item,
        write: WriteCallback,
        write_state: WriteCallbackState,
    ) -> Result<Option<Self::Report>> {
        let mut chunk_report = None::<Self::Report>;
        let crate_versions = db.open_crate_versions()?;
        let mut reports_to_mark_done = Vec::new();
        let mut out_buf = Vec::new();
        {
            let connection = db.open_connection()?;
            let reports = db.open_reports()?;
            let mut key_buf = String::with_capacity(32);
            // delaying writes works because we don't have overlap on work
            for (name, krate) in krates.into_iter() {
                let c: model::Crate = krate.as_slice().into();
                let crate_dir = crate_dir(&out_dir, &name);
                progress.init(Some(c.versions.len()), Some("versions".into()));
                progress.set_name(&name);

                let mut crate_report = None::<Self::Report>;
                for version in c.versions.iter().take(all_but_recently_yanked(
                    &name,
                    &c.versions,
                    &crate_versions,
                    &mut key_buf,
                )?) {
                    progress.inc();

                    key_buf.clear();
                    Self::fq_report_key(&name, version, &mut key_buf);

                    // If we have no cache, assume we are globbed (yes, I know…sigh), so always produce reports
                    // but don't invalidate data in caches by reading or writing them. Mostly used for testing
                    // as it creates a sub-report, every time without having to fiddle with the
                    // reports_done marker table.
                    if cache_dir.is_none() || !reports.is_done(&key_buf) {
                        let reports_key = key_buf.clone();
                        key_buf.clear();

                        if let Some(result) = Self::get_result(connection.clone(), &name, version, &mut key_buf)? {
                            let mut version_report =
                                Self::generate_report(&name, version, result, &mut progress).await?;

                            out_buf = complete_and_write_report(
                                &mut version_report,
                                out_buf,
                                &mut progress,
                                version_html_path(&crate_dir, version),
                                write,
                                &write_state,
                            )
                            .await?;

                            crate_report = Some(match crate_report {
                                Some(crate_report) => crate_report.merge(version_report),
                                None => version_report,
                            });

                            reports_to_mark_done.push(reports_key);
                        }
                    }
                }
                if let Some(mut crate_report) = crate_report {
                    let previous_state = match cache_dir.as_ref() {
                        Some(cd) => crate_report.load_previous_state(cd, &mut progress).await,
                        None => None,
                    };
                    match previous_state {
                        Some(previous_state) => {
                            let mut absolute_state = previous_state.merge(crate_report.clone());
                            out_buf = complete_and_write_report(
                                &mut absolute_state,
                                out_buf,
                                &mut progress,
                                crate_html_path(&crate_dir),
                                write,
                                &write_state,
                            )
                            .await?;
                            if let Some(cd) = cache_dir.as_ref() {
                                absolute_state.store_current_state(cd, &mut progress).await?;
                            };
                        }
                        None => {
                            out_buf = complete_and_write_report(
                                &mut crate_report,
                                out_buf,
                                &mut progress,
                                crate_html_path(&crate_dir),
                                write,
                                &write_state,
                            )
                            .await?;
                            if let Some(cd) = cache_dir.as_ref() {
                                crate_report.store_current_state(cd, &mut progress).await?;
                            }
                        }
                    }
                    chunk_report = Some(match chunk_report {
                        Some(chunk_report) => chunk_report.merge(crate_report),
                        None => crate_report,
                    });
                }
            }
        }

        if !reports_to_mark_done.is_empty() {
            let mut connection = db.open_connection_no_async_with_busy_wait()?;
            progress.blocked("wait for write lock", None);
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            progress.init(
                Some(reports_to_mark_done.len()),
                Some("report done markers written".into()),
            );
            {
                let mut statement = new_key_insertion(ReportsTree::table_name(), &transaction)?;
                for key in reports_to_mark_done.iter() {
                    statement.execute(params![key])?;
                    progress.inc();
                }
            }
            transaction.commit()?;
        }
        Ok(chunk_report)
    }
}

fn crate_dir(base: &Path, crate_name: &str) -> PathBuf {
    base.join(crate_name)
}

fn version_html_path(crate_dir: &Path, version: &str) -> PathBuf {
    crate_dir.join(format!("{}.html", version))
}
fn crate_html_path(crate_dir: &Path) -> PathBuf {
    crate_dir.join("index.html")
}

async fn complete_and_write_report(
    report: &mut impl Aggregate,
    mut out: Vec<u8>,
    progress: &mut prodash::tree::Item,
    path: impl AsRef<Path>,
    write: WriteCallback,
    write_state: &WriteCallbackState,
) -> Result<Vec<u8>> {
    out.clear();
    report.complete(progress, &mut out).await?;
    progress.blocked("sending report to writer", None);
    match write(
        WriteRequest {
            path: path.as_ref().to_path_buf(),
            content: out,
        },
        write_state,
    )
    .await?
    {
        WriteInstruction::DoWrite(WriteRequest { path, content }) => {
            blocking::unblock({
                let path = path.clone();
                move || std::fs::create_dir_all(path.parent().expect("file path with parent directory"))
            })
            .await?;
            progress.halted("writing report to disk", None);

            let content = blocking::unblock(move || std::fs::write(path, &content).map(|_| content)).await?;
            Ok(content)
        }
        WriteInstruction::Skip => Ok(Vec::new()),
    }
}
