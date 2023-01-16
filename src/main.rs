use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use tokio::fs::File;

use rocket::form::Form;
use rocket::fs::TempFile;
use rocket_db_pools::{Connection, Database};

use sqlx::types::Uuid;
use sqlx::Error as SqlError;

use rocket::data::Capped;
use rocket::fairing::AdHoc;
use rocket::http::{ContentType, Header};
use rocket::request::{FromParam, Request};
use rocket::response::{self, Responder};

use rocket::http::uri::fmt::{FromUriParam, Path as UriPath};
use rocket::http::uri::Absolute;

use rocket::{catch, catchers, get, launch, post, routes, uri, FromForm, State};
use serde::Deserialize;

static FILE_PATH: &'static str = "/tmp/test";

#[derive(Deserialize)]
struct AppConfig<'a> {
    file_path: String,
    base_url: Absolute<'a>,
}

fn to_ipv6(addr: IpAddr) -> Ipv6Addr {
    match addr {
        IpAddr::V4(a) => a.to_ipv6_mapped(),
        IpAddr::V6(a) => a,
    }
}

fn bytes_to_ipv6(bytes: Vec<u8>) -> Option<Ipv6Addr> {
    if bytes.len() != 16 {
        return None;
    }

    let mut b = [0u8; 16];
    b.copy_from_slice(&bytes);

    Some(Ipv6Addr::from(b))
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
struct FileDescriptor(Uuid);

impl From<Uuid> for FileDescriptor {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl AsRef<Uuid> for FileDescriptor {
    fn as_ref(&self) -> &Uuid {
        &self.0
    }
}

impl ToString for FileDescriptor {
    fn to_string(&self) -> String {
        self.0.to_hyphenated_ref().to_string()
    }
}

enum FileDescriptorParamError {
    Uuid(sqlx::types::uuid::Error),
    InvalidChars,
}

impl std::fmt::Debug for FileDescriptorParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uuid(_) => write!(f, "Invalid UUID"),
            Self::InvalidChars => write!(f, "Invalid characters in UUID"),
        }
    }
}

impl<'r> FromParam<'r> for FileDescriptor {
    type Error = FileDescriptorParamError;
    fn from_param(param: &'r str) -> Result<Self, Self::Error> {
        if param.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            Uuid::parse_str(param)
                .map(|u| u.into())
                .map_err(FileDescriptorParamError::Uuid)
        } else {
            Err(FileDescriptorParamError::InvalidChars)
        }
    }
}

impl FromUriParam<UriPath, FileDescriptor> for FileDescriptor {
    type Target = String;

    fn from_uri_param(param: FileDescriptor) -> Self::Target {
        param.to_string()
    }
}

struct FileDownload(File, FileInfo);

impl FileDownload {
    pub async fn open(fd: FileInfo, file_path: &str) -> std::io::Result<Self> {
        let path = Path::new(&file_path).join(fd.fd.to_string());
        let file = File::open(path).await?;
        Ok(Self(file, fd))
    }
}

impl<'r> Responder<'r, 'static> for FileDownload {
    fn respond_to(self, r: &'r Request<'_>) -> response::Result<'static> {
        let mut res = self.0.respond_to(r)?;

        res.set_header(ContentType::Binary);

        let filename = self.1.name.unwrap_or_else(|| self.1.fd.to_string());

        res.set_header(Header::new(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        ));

        Ok(res)
    }
}

#[derive(Database)]
#[database("file_index")]
struct FileIndex(rocket_db_pools::sqlx::SqlitePool);

struct FileInfo {
    fd: FileDescriptor,
    name: Option<String>,
    upload_ip: Ipv6Addr,
}

impl FileInfo {
    pub fn new(fd: FileDescriptor, name: Option<String>, upload_ip: Ipv6Addr) -> Self {
        Self {
            fd,
            name,
            upload_ip,
        }
    }
}

async fn get_file(
    mut db: Connection<FileIndex>,
    fd: FileDescriptor,
) -> Result<Option<FileInfo>, SqlError> {
    let f = fd.as_ref();
    let info = match sqlx::query!("SELECT name, upload_ip FROM file_index WHERE fd = ?1", f)
        .fetch_one(&mut *db)
        .await
    {
        Ok(i) => i,
        Err(SqlError::RowNotFound) => return Ok(None),
        Err(e) => return Err(e),
    };

    let name = info.name;
    let addr = match bytes_to_ipv6(info.upload_ip) {
        Some(a) => a,
        None => return Ok(None),
    };

    Ok(Some(FileInfo::new(fd, name, addr)))
}

async fn add_file(mut db: Connection<FileIndex>, info: FileInfo) -> Result<(), FileError> {
    let f = info.fd.as_ref();
    let upload_ip = info.upload_ip.octets().to_vec();
    sqlx::query!(
        "INSERT INTO file_index (fd, name, upload_ip) VALUES (?1, ?2, ?3)",
        f,
        info.name,
        upload_ip
    )
    .execute(&mut *db)
    .await?;

    Ok(())
}

#[derive(Responder)]
enum FileError {
    #[response(status = 500)]
    SqlError(&'static str, #[response(ignore)] SqlError),
    #[response(status = 500)]
    IoError(&'static str, #[response(ignore)] std::io::Error),
    #[response(status = 413)]
    FileTooBig(&'static str),
    #[response(status = 400)]
    Uuid(&'static str),
    #[response(status = 422)]
    InvalidForm(&'static str),
}

impl From<std::io::Error> for FileError {
    fn from(e: std::io::Error) -> Self {
        Self::IoError("EIO: I/O error\n", e)
    }
}

impl From<SqlError> for FileError {
    fn from(e: SqlError) -> Self {
        Self::SqlError("EROFS: Read-only file system\n", e)
    }
}

async fn start_file_download(
    db: Connection<FileIndex>,
    fd: FileDescriptor,
    name: Option<String>,
    file_path: &str,
) -> Result<Option<FileDownload>, FileError> {
    Ok(if let Some(mut file) = get_file(db, fd).await? {
        if let Some(n) = name {
            file.name = Some(n);
        }
        Some(FileDownload::open(file, file_path).await?)
    } else {
        None
    })
}

#[get("/fd/<fd>")]
async fn download_file(
    db: Connection<FileIndex>,
    fd: FileDescriptor,
    config: &State<AppConfig<'_>>,
) -> Result<Option<FileDownload>, FileError> {
    start_file_download(db, fd, None, &config.file_path).await
}

#[get("/fd/<fd>/<name>")]
async fn download_file_named(
    db: Connection<FileIndex>,
    fd: FileDescriptor,
    name: String,
    config: &State<AppConfig<'_>>,
) -> Result<Option<FileDownload>, FileError> {
    start_file_download(db, fd, Some(name), &config.file_path).await
}

#[get("/fd/<_>", rank = 2)]
async fn download_file_invalid_fd() -> FileError {
    FileError::Uuid("EINVAL: invalid argument\n")
}

async fn upload_file(
    db: Connection<FileIndex>,
    name: Option<String>,
    file: &mut Capped<TempFile<'_>>,
    addr: IpAddr,
    base_url: &Absolute<'_>,
) -> Result<Option<String>, FileError> {
    if !file.is_complete() {
        return Err(FileError::FileTooBig("ENOSPC: No space left on device"));
    }

    let fd: FileDescriptor = Uuid::new_v4().into();

    let path = Path::new(FILE_PATH).join(fd.to_string());
    file.move_copy_to(path.clone()).await?;

    let file = FileInfo::new(fd, name, to_ipv6(addr));
    add_file(db, file).await?;

    Ok(Some(format!(
        "{}\n",
        uri!(base_url.clone(), download_file(fd))
    )))
}

#[post("/raw", format = "application/x-www-form-urlencoded", data = "<file>")]
async fn upload_file_raw(
    db: Connection<FileIndex>,
    mut file: Capped<TempFile<'_>>,
    addr: IpAddr,
    config: &State<AppConfig<'_>>,
) -> Result<Option<String>, FileError> {
    upload_file(db, None, &mut file, addr, &config.base_url).await
}

#[post("/raw", format = "multipart/form-data", rank = 2)]
async fn upload_file_raw_invalid() -> FileError {
    FileError::InvalidForm("EINVAL: invalid argument\n")
}

#[derive(FromForm)]
struct FileDescriptorForm<'r> {
    file: Capped<TempFile<'r>>,
    name: Option<String>,
}

#[post("/", data = "<form>")]
async fn upload_file_form(
    db: Connection<FileIndex>,
    mut form: Form<FileDescriptorForm<'_>>,
    addr: IpAddr,
    config: &State<AppConfig<'_>>,
) -> Result<Option<String>, FileError> {
    upload_file(
        db,
        form.name.clone(),
        &mut form.file,
        addr,
        &config.base_url,
    )
    .await
}

#[catch(404)]
fn not_found(req: &Request) -> String {
    format!(
        "thread 'rocket-worker-thread' panicked at 'ENOENT: No such file or directory \"{}\"', src/main.rs:267:4\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n",
        req.uri()
    )
}

#[catch(404)]
fn file_not_found(req: &Request) -> String {
    format!(
        "ENOENT: No such file or directory: \"{}\"\n",
        &req.uri().to_string()[4..]
    )
}

#[catch(422)]
fn invalid_form() -> &'static str {
    "EINVAL: Invalid argument"
}

#[catch(default)]
fn server_error() -> &'static str {
    "fish: Job 1, 'cargo run --release' terminated by signal SIGSEGV (Address boundary error)"
}

#[launch]
fn rocket() -> _ {
    rocket::build()
        .mount(
            "/",
            routes![
                download_file,
                download_file_named,
                download_file_invalid_fd,
                upload_file_raw,
                upload_file_raw_invalid,
                upload_file_form
            ],
        )
        .register("/", catchers![not_found, invalid_form, server_error])
        .register("/fd", catchers![file_not_found])
        .attach(FileIndex::init())
        .attach(AdHoc::config::<AppConfig>())
}
