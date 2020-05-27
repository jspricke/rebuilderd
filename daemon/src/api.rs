use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use rebuilderd_common::errors::*;
use chrono::prelude::*;
use crate::auth;
use crate::config::Config;
use crate::models;
use rebuilderd_common::api::*;
use rebuilderd_common::PkgRelease;
use crate::db::Pool;
use crate::sync;
use diesel::SqliteConnection;

fn forbidden() -> Result<HttpResponse> {
    Ok(HttpResponse::Forbidden()
        .body("Authentication failed\n"))
}

pub fn header<'a>(req: &'a HttpRequest, key: &str) -> Result<&'a str> {
    let value = req.headers().get(key)
        .ok_or_else(|| format_err!("Missing header"))?
        .to_str()
        .context("Failed to decode header value")?;
    Ok(value)
}

#[get("/api/v0/workers")]
pub async fn list_workers(
    req: HttpRequest,
    cfg: web::Data<Config>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::admin(&cfg, &req).is_err() {
        return forbidden();
    }

    let connection = pool.get()?;
    models::Worker::mark_stale_workers_offline(connection.as_ref())?;
    let workers = models::Worker::list(connection.as_ref())?;
    Ok(HttpResponse::Ok().json(workers))
}

// this route is configured in src/main.rs so we can reconfigure the json extractor
// #[post("/api/v0/job/sync")]
pub async fn sync_work(
    req: HttpRequest,
    cfg: web::Data<Config>,
    import: web::Json<SuiteImport>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::admin(&cfg, &req).is_err() {
        return forbidden();
    }

    let import = import.into_inner();
    let connection = pool.get()?;

    sync::run(import, connection.as_ref())?;

    Ok(HttpResponse::Ok().json(JobAssignment::Nothing))
}

fn opt_filter(this: &str, filter: Option<&str>) -> bool {
    if let Some(filter) = filter {
        if this != filter {
            return true;
        }
    }
    false
}

#[get("/api/v0/pkgs/list")]
pub async fn list_pkgs(
    query: web::Query<ListPkgs>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    let connection = pool.get()?;

    let mut pkgs = Vec::<PkgRelease>::new();
    for pkg in models::Package::list(connection.as_ref())? {
        if opt_filter(&pkg.name, query.name.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.status, query.status.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.distro, query.distro.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.suite, query.suite.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.architecture, query.architecture.as_deref()) {
            continue;
        }

        pkgs.push(pkg.into_api_item()?);
    }

    Ok(HttpResponse::Ok().json(pkgs))
}

#[post("/api/v0/queue/list")]
pub async fn list_queue(
    query: web::Json<ListQueue>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    let connection = pool.get()?;

    models::Queued::free_stale_jobs(connection.as_ref())?;
    let queue = models::Queued::list(query.limit, connection.as_ref())?;
    let queue: Vec<QueueItem> = queue.into_iter()
        .map(|x| x.into_api_item(connection.as_ref()))
        .collect::<Result<_>>()?;

    let now = Utc::now().naive_utc();

    Ok(HttpResponse::Ok().json(QueueList {
        now,
        queue,
    }))
}

fn get_worker_from_request(req: &HttpRequest, connection: &SqliteConnection) -> Result<models::Worker> {
    let key = header(req, WORKER_KEY_HEADER)
        .context("Failed to get worker key")?;

    let ci = req.peer_addr()
        .ok_or_else(|| format_err!("Can't determine client ip"))?;

    if let Some(mut worker) = models::Worker::get(key, connection)? {
        worker.bump_last_ping();
        Ok(worker)
    } else {
        let worker = models::NewWorker::new(key.to_string(), ci.ip(), None);
        worker.insert(connection)?;
        get_worker_from_request(req, connection)
    }
}

#[post("/api/v0/queue/push")]
pub async fn push_queue(
    req: HttpRequest,
    cfg: web::Data<Config>,
    query: web::Json<PushQueue>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::admin(&cfg, &req).is_err() {
        return forbidden();
    }

    let query = query.into_inner();
    let connection = pool.get()?;

    debug!("searching pkg: {:?}", query);
    let pkgs = models::Package::get_by(&query.name, &query.distro, &query.suite, query.architecture.as_deref(), connection.as_ref())?;

    for pkg in pkgs {
        debug!("found pkg: {:?}", pkg);
        let version = query.version.as_ref().unwrap_or(&pkg.version);

        let item = models::NewQueued::new(pkg.id, version.to_string());
        debug!("adding to queue: {:?}", item);
        item.insert(connection.as_ref())?;
    }

    Ok(HttpResponse::Ok().json(()))
}

#[post("/api/v0/queue/pop")]
pub async fn pop_queue(
    req: HttpRequest,
    cfg: web::Data<Config>,
    _query: web::Json<WorkQuery>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::worker(&cfg, &req).is_err() {
        return forbidden();
    }

    let connection = pool.get()?;

    let mut worker = get_worker_from_request(&req, connection.as_ref())?;

    models::Queued::free_stale_jobs(connection.as_ref())?;
    let (resp, status) = if let Some(item) = models::Queued::pop_next(worker.id, connection.as_ref())? {


        // TODO: claim item correctly


        let status = format!("working hard on {} {}", item.package.name, item.package.version);
        (JobAssignment::Rebuild(item), Some(status))
    } else {
        (JobAssignment::Nothing, None)
    };

    worker.status = status;
    worker.update(connection.as_ref())?;

    Ok(HttpResponse::Ok().json(resp))
}

#[post("/api/v0/queue/drop")]
pub async fn drop_from_queue(
    req: HttpRequest,
    cfg: web::Data<Config>,
    query: web::Json<DropQueueItem>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::admin(&cfg, &req).is_err() {
        return forbidden();
    }

    let query = query.into_inner();
    let connection = pool.get()?;

    let pkgs = models::Package::get_by(&query.name, &query.distro, &query.suite, query.architecture.as_deref(), connection.as_ref())?;
    let pkgs = pkgs.iter()
        .map(|p| p.id)
        .collect::<Vec<_>>();

    models::Queued::drop_for_pkgs(&pkgs, connection.as_ref())?;

    Ok(HttpResponse::Ok().json(()))
}

#[post("/api/v0/pkg/requeue")]
pub async fn requeue_pkg(
    req: HttpRequest,
    cfg: web::Data<Config>,
    query: web::Json<RequeueQuery>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::admin(&cfg, &req).is_err() {
        return forbidden();
    }

    let connection = pool.get()?;

    let mut pkgs = Vec::new();
    for pkg in models::Package::list(connection.as_ref())? {
        if opt_filter(&pkg.name, query.name.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.status, query.status.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.distro, query.distro.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.suite, query.suite.as_deref()) {
            continue;
        }
        if opt_filter(&pkg.architecture, query.architecture.as_deref()) {
            continue;
        }

        debug!("pkg is going to be requeued: {:?} {:?}", pkg.name, pkg.version);
        pkgs.push((pkg.id, pkg.version));
    }

    // TODO: use queue_batch after https://github.com/diesel-rs/diesel/pull/1884 is released
    // models::Queued::queue_batch(&pkgs, connection.as_ref())?;
    for (id, version) in &pkgs {
        let q = models::NewQueued::new(*id, version.to_string());
        q.insert(connection.as_ref()).ok();
    }

    if query.reset {
        let reset = pkgs.into_iter()
            .map(|x| x.0)
            .collect::<Vec<_>>();
        models::Package::reset_status_for_requeued_list(&reset, connection.as_ref())?;
    }

    Ok(HttpResponse::Ok().json(()))
}

#[post("/api/v0/build/ping")]
pub async fn ping_build(
    req: HttpRequest,
    cfg: web::Data<Config>,
    item: web::Json<QueueItem>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::worker(&cfg, &req).is_err() {
        return forbidden();
    }

    let connection = pool.get()?;

    let worker = get_worker_from_request(&req, connection.as_ref())?;
    debug!("ping from worker: {:?}", worker);
    let mut item = models::Queued::get_id(item.id, connection.as_ref())?;
    debug!("trying to ping item: {:?}", item);

    if item.worker_id != Some(worker.id) {
        bail!("Trying to write to item we didn't assign")
    }

    debug!("updating database (item)");
    item.ping_job(connection.as_ref())?;
    debug!("updating database (worker)");
    worker.update(connection.as_ref())?;
    debug!("successfully pinged job");

    Ok(HttpResponse::Ok().json(()))
}

#[post("/api/v0/build/report")]
pub async fn report_build(
    req: HttpRequest,
    cfg: web::Data<Config>,
    report: web::Json<BuildReport>,
    pool: web::Data<Pool>,
) -> Result<impl Responder> {
    if auth::worker(&cfg, &req).is_err() {
        return forbidden();
    }

    let connection = pool.get()?;

    let mut worker = get_worker_from_request(&req, connection.as_ref())?;
    let item = models::Queued::get_id(report.queue.id, connection.as_ref())?;
    let mut pkg = models::Package::get_id(item.package_id, connection.as_ref())?;

    pkg.update_status_safely(&report.rebuild, connection.as_ref())?;
    item.delete(connection.as_ref())?;

    worker.status = None;
    worker.update(connection.as_ref())?;

    Ok(HttpResponse::Ok().json(()))
}
