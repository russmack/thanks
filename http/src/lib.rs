extern crate futures;
extern crate hyper;
extern crate regex;
extern crate reqwest;
extern crate serde_json;
extern crate handlebars;
extern crate futures_cpupool;

#[macro_use]
extern crate slog;
extern crate slog_term;

use hyper::StatusCode;
use hyper::header::{ContentType, Location};
use hyper::server::{Http, Service};

use handlebars::Handlebars;

use regex::{Regex, Captures};

use std::io::prelude::*;
use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;

use slog::DrainExt;

use futures::future::Future;
use futures::future::FutureResult;
use futures::BoxFuture;

use futures_cpupool::CpuPool;

use serde_json::value::Value;
// Rename type for crate
type BTreeMap = std::collections::BTreeMap<String, Value>;

pub struct Request {
    request: hyper::server::Request,
}

pub struct Response {
    data: BTreeMap,
    template: String,
    status: Status,
}

pub struct Error {
    inner: hyper::Error,
}

pub struct ResponseBuilder {
    pub data: BTreeMap,
    template: Option<String>,
    status: Option<Status>,
}

impl ResponseBuilder {
    pub fn new() -> ResponseBuilder {
        ResponseBuilder {
            data: BTreeMap::new(),
            template: None,
            status: None,
        }
    }

    pub fn with_template(&mut self, template: String) {
        self.template = Some(template);
    }

    pub fn with_status(&mut self, status: Status) {
        self.status = Some(status);
    }

    pub fn to_response(self) -> Response {
        Response {
            data: self.data,
            template: self.template.unwrap(),
            status: self.status.unwrap(),
        }
    }
}

pub enum Status {
    Ok,
    NotFound,
}

pub struct Server {
    routes: Vec<Route>,
    catch_all_route: Option<fn(Request) -> BoxFuture<Response, Error>>,
    template_root: String,
    log: slog::Logger,
    pool: CpuPool,
}

pub enum Route {
    Literal {
        path: String,
        handler: fn(Request) -> BoxFuture<Response, Error>,
    },
    Regex {
        regex: Regex,
        handler: fn(&Request, Captures) -> BoxFuture<Response, Error>,
    },
}

impl Route {
    fn matches(&self, path: &str) -> bool {
        match self {
            &Route::Literal { path: ref p, .. } => {
                p == path
            },
            &Route::Regex { ref regex, .. } => {
                regex.is_match(path)
            },
        }
    }

    fn handle(&self, req: Request) -> BoxFuture<Response, Error> {
        match self {
            &Route::Literal { handler, .. } => {
                handler(req)
            },
            &Route::Regex { handler, ref regex } => {
                // i am extremely suspicous of this unwrap
                let captures = regex.captures(req.request.path()).unwrap();

                handler(&req, captures)
            },
        }
    }
}

impl Server {
    pub fn new(template_root: String) -> Server {
        Server {
            routes: Vec::new(),
            catch_all_route: None,
            template_root: template_root,
            log: slog::Logger::root(slog_term::streamer().full().build().fuse(), o!()),
            pool: CpuPool::new(4), // FIXME: is this right? who knows!
        }
    }

    pub fn add_route(&mut self, path: &str, handler: fn(Request) -> BoxFuture<Response, Error>) {
        let path = path.to_string();

        self.routes.push(Route::Literal {
            path: path,
            handler: handler,
        });
    }

    pub fn add_regex_route(&mut self, regex: &str, handler: fn(&Request, Captures) -> BoxFuture<Response, Error>) {
        self.routes.push(Route::Regex {
            regex: Regex::new(regex).unwrap(),
            handler: handler,
        });
    }

    pub fn add_catch_all_route(&mut self, f: fn(Request) -> BoxFuture<Response, Error>) {
        self.catch_all_route = Some(f);
    }

    pub fn run(self, addr: &SocketAddr) {
        info!(self.log, "Starting server, listening on http://{}", addr);

        let a = std::sync::Arc::new(self);

        let server = Http::new().bind(addr, move || Ok(a.clone())).unwrap();


        server.run().unwrap();
    }
}

impl Service for Server {
    type Request = hyper::server::Request;
    type Response = hyper::server::Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: hyper::server::Request) -> Self::Future {
        // redirect to ssl
        // from http://jaketrent.com/post/https-redirect-node-heroku/
        if let Some(raw) = req.headers().get_raw("x-forwarded-proto") {
            if raw != &b"https"[..] {
                return ::futures::future::ok(
                    hyper::server::Response::new()
                    .with_header(Location(format!("https://thanks.rust-lang.org{}", req.path())))
                    .with_status(StatusCode::MovedPermanently)
                ).boxed();
            }
        }

        // first, we serve static files
        let fs_path = format!("public{}", req.path());

        // ... you trying to do something bad?
        if fs_path.contains("./") || fs_path.contains("../") {
            // GET OUT
            return ::futures::future::ok(hyper::server::Response::new()
                .with_header(ContentType::html())
                .with_status(StatusCode::NotFound))
                .boxed();
        }

        if Path::new(&fs_path).is_file() {
            return self.pool.spawn_fn(move || {
                let mut f = File::open(&fs_path).unwrap();

                let mut source = Vec::new();

                f.read_to_end(&mut source).unwrap();

                futures::future::ok(hyper::server::Response::new()
                    .with_body(source))
            }).boxed();
        }

        // next, we check routes
        
        for route in &self.routes {
            if route.matches(req.path()) {
                let r = Request {
                    request: req,
                };
                let template_root = self.template_root.clone();
                let response = route.handle(r).and_then(move |response| {
                    match response.status {
                        Status::Ok=> {
                            let body = build_template(&template_root, &response.data, &response.template);

                            futures::future::ok(hyper::server::Response::new()
                                .with_header(ContentType::html())
                                .with_body(body))
                        }
                        Status::NotFound => {
                            ::futures::future::ok(hyper::server::Response::new().with_status(StatusCode::NotFound))
                        }
                    }
                }).map_err(|e| e.inner);

                return response.boxed();
            }
        }

        if let Some(h) = self.catch_all_route {
            let r = Request {
                request: req,
            };
            let template_root = self.template_root.clone();
            let response = h(r).and_then(move |response| {
                match response.status {
                    Status::Ok => {
                        let body = build_template(&template_root, &response.data, &response.template);

                        ::futures::future::ok(hyper::server::Response::new()
                            .with_header(ContentType::html())
                            .with_body(body))
                    }
                    Status::NotFound => {
                        ::futures::future::ok(hyper::server::Response::new().with_status(StatusCode::NotFound))
                    }
                }
            }).map_err(|e| e.inner);

            return response.boxed();
        }

        ::futures::future::ok(hyper::server::Response::new()
                            .with_header(ContentType::html())
                            .with_status(StatusCode::NotFound)).boxed()
    }
}

fn build_template(template_root: &str, data: &BTreeMap, template_path: &str) -> String {
    let mut handlebars = Handlebars::new();
    // Render the partials
    handlebars.register_template_file("container", &Path::new(&format!("{}/container.hbs", template_root)))
        .ok()
        .unwrap();
    handlebars.register_template_file("index", &Path::new(&format!("{}/{}", template_root, template_path)))
        .ok()
        .unwrap();
    let mut data = data.clone();
    // Add name of the container to be loaded (just a constant for now)
    data.insert("parent".to_string(), Value::String("container".to_string()));

    // That's all we need to build this thing
    handlebars.render("index", &data).unwrap()
}
