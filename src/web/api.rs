//! Defines Rojo's HTTP API, all under /api. These endpoints generally return
//! JSON.

use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::Arc};

use futures::{Future, Stream};

use fs_err as fs;
use hyper::{service::Service, Body, Method, Request, StatusCode};
use rbx_dom_weak::{types::Ref, InstanceBuilder, WeakDom};
use roblox_install::RobloxStudio;
use uuid::Uuid;

use crate::{
    serve_session::ServeSession,
    snapshot::{InstanceWithMeta, PatchSet, PatchUpdate},
    web::{
        interface::{
            ErrorResponse, Instance, InstanceMetadata as WebInstanceMetadata, InstanceUpdate,
            OpenResponse, ReadResponse, ServerInfoResponse, SubscribeMessage, SubscribeResponse,
            WriteRequest, WriteResponse, PROTOCOL_VERSION, SERVER_VERSION,
        },
        util::{json, json_ok},
    },
    web_api::{CreateAssetsRequest, CreateAssetsResponse},
};

const CREATE_ASSETS_DIR: &str = "rojo-exports";

pub struct ApiService {
    serve_session: Arc<ServeSession>,
}

impl Service for ApiService {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = hyper::Error;
    type Future =
        Box<dyn Future<Item = hyper::Response<Self::ReqBody>, Error = Self::Error> + Send>;

    fn call(&mut self, request: hyper::Request<Self::ReqBody>) -> Self::Future {
        match (request.method(), request.uri().path()) {
            (&Method::GET, "/api/rojo") => self.handle_api_rojo(),
            (&Method::GET, path) if path.starts_with("/api/read/") => self.handle_api_read(request),
            (&Method::GET, path) if path.starts_with("/api/subscribe/") => {
                self.handle_api_subscribe(request)
            }
            (&Method::POST, path) if path.starts_with("/api/open/") => {
                self.handle_api_open(request)
            }

            (&Method::POST, "/api/write") => self.handle_api_write(request),
            (&Method::POST, "/api/create-assets") => self.handle_api_create_assets(request),

            (_method, path) => json(
                ErrorResponse::not_found(format!("Route not found: {}", path)),
                StatusCode::NOT_FOUND,
            ),
        }
    }
}

impl ApiService {
    pub fn new(serve_session: Arc<ServeSession>) -> Self {
        ApiService { serve_session }
    }

    /// Get a summary of information about the server
    fn handle_api_rojo(&self) -> <Self as Service>::Future {
        let tree = self.serve_session.tree();
        let root_instance_id = tree.get_root_id();

        json_ok(&ServerInfoResponse {
            server_version: SERVER_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            session_id: self.serve_session.session_id(),
            project_name: self.serve_session.project_name().to_owned(),
            expected_place_ids: self.serve_session.serve_place_ids().cloned(),
            root_instance_id,
        })
    }

    /// Retrieve any messages past the given cursor index, and if
    /// there weren't any, subscribe to receive any new messages.
    fn handle_api_subscribe(&self, request: Request<Body>) -> <Self as Service>::Future {
        let argument = &request.uri().path()["/api/subscribe/".len()..];
        let input_cursor: u32 = match argument.parse() {
            Ok(v) => v,
            Err(err) => {
                return json(
                    ErrorResponse::bad_request(format!("Malformed message cursor: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let session_id = self.serve_session.session_id();

        let receiver = self.serve_session.message_queue().subscribe(input_cursor);

        let tree_handle = self.serve_session.tree_handle();

        Box::new(receiver.then(move |result| match result {
            Ok((message_cursor, messages)) => {
                let tree = tree_handle.lock().unwrap();

                let api_messages = messages
                    .into_iter()
                    .map(|message| {
                        let removed = message.removed;

                        let mut added = HashMap::new();
                        for id in message.added {
                            let instance = tree.get_instance(id).unwrap();
                            added.insert(id, Instance::from_rojo_instance(instance));

                            for instance in tree.descendants(id) {
                                added.insert(instance.id(), Instance::from_rojo_instance(instance));
                            }
                        }

                        let updated = message
                            .updated
                            .into_iter()
                            .map(|update| {
                                let changed_metadata = update
                                    .changed_metadata
                                    .as_ref()
                                    .map(WebInstanceMetadata::from_rojo_metadata);

                                InstanceUpdate {
                                    id: update.id,
                                    changed_name: update.changed_name,
                                    changed_class_name: update.changed_class_name,
                                    changed_properties: update.changed_properties,
                                    changed_metadata,
                                }
                            })
                            .collect();

                        SubscribeMessage {
                            removed,
                            added,
                            updated,
                        }
                    })
                    .collect();

                json_ok(SubscribeResponse {
                    session_id,
                    message_cursor,
                    messages: api_messages,
                })
            }
            Err(_) => json(
                ErrorResponse::internal_error("Message queue disconnected sender"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }))
    }

    fn handle_api_write(&self, request: Request<Body>) -> <Self as Service>::Future {
        let session_id = self.serve_session.session_id();
        let tree_mutation_sender = self.serve_session.tree_mutation_sender();

        Box::new(request.into_body().concat2().and_then(move |body| {
            let request: WriteRequest = match serde_json::from_slice(&body) {
                Ok(request) => request,
                Err(err) => {
                    return json(
                        ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                        StatusCode::BAD_REQUEST,
                    );
                }
            };

            if request.session_id != session_id {
                return json(
                    ErrorResponse::bad_request("Wrong session ID"),
                    StatusCode::BAD_REQUEST,
                );
            }

            let updated_instances = request
                .updated
                .into_iter()
                .map(|update| PatchUpdate {
                    id: update.id,
                    changed_class_name: update.changed_class_name,
                    changed_name: update.changed_name,
                    changed_properties: update.changed_properties,
                    changed_metadata: None,
                })
                .collect();

            tree_mutation_sender
                .send(PatchSet {
                    removed_instances: Vec::new(),
                    added_instances: Vec::new(),
                    updated_instances,
                })
                .unwrap();

            json_ok(&WriteResponse { session_id })
        }))
    }

    fn handle_api_read(&self, request: Request<Body>) -> <Self as Service>::Future {
        let argument = &request.uri().path()["/api/read/".len()..];
        let requested_ids: Result<Vec<Ref>, _> = argument.split(',').map(Ref::from_str).collect();

        let requested_ids = match requested_ids {
            Ok(ids) => ids,
            Err(_) => {
                return json(
                    ErrorResponse::bad_request("Malformed ID list"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let message_queue = self.serve_session.message_queue();
        let message_cursor = message_queue.cursor();

        let tree = self.serve_session.tree();

        let mut instances = HashMap::new();

        for id in requested_ids {
            if let Some(instance) = tree.get_instance(id) {
                instances.insert(id, Instance::from_rojo_instance(instance));

                for descendant in tree.descendants(id) {
                    instances.insert(descendant.id(), Instance::from_rojo_instance(descendant));
                }
            }
        }

        json_ok(ReadResponse {
            session_id: self.serve_session.session_id(),
            message_cursor,
            instances,
        })
    }

    /// Open a script with the given ID in the user's default text editor.
    fn handle_api_open(&self, request: Request<Body>) -> <Self as Service>::Future {
        let argument = &request.uri().path()["/api/open/".len()..];
        let requested_id = match Ref::from_str(argument) {
            Ok(id) => id,
            Err(_) => {
                return json(
                    ErrorResponse::bad_request("Invalid instance ID"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let tree = self.serve_session.tree();

        let instance = match tree.get_instance(requested_id) {
            Some(instance) => instance,
            None => {
                return json(
                    ErrorResponse::bad_request("Instance not found"),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        let script_path = match pick_script_path(instance) {
            Some(path) => path,
            None => {
                return json(
                    ErrorResponse::bad_request(
                        "No appropriate file could be found to open this script",
                    ),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        let _ = opener::open(script_path);

        json_ok(&OpenResponse {
            session_id: self.serve_session.session_id(),
        })
    }

    fn handle_api_create_assets(&self, request: Request<Body>) -> <Self as Service>::Future {
        let session_id = self.serve_session.session_id();

        let serve_tree = self.serve_session.tree_handle();

        Box::new(request.into_body().concat2().and_then(move |body| {
            let serve_tree = serve_tree.lock().expect("Couldn't lock RojoTree mutex");

            let request: CreateAssetsRequest = match serde_json::from_slice(&body) {
                Ok(request) => request,
                Err(err) => {
                    return json(
                        ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                        StatusCode::BAD_REQUEST,
                    );
                }
            };

            if request.session_id != session_id {
                return json(
                    ErrorResponse::bad_request("Wrong session ID"),
                    StatusCode::BAD_REQUEST,
                );
            }

            let serve_dom = serve_tree.inner();

            let model_path = format!("{}.rbxm", Uuid::new_v4().to_simple());

            let studio = match RobloxStudio::locate() {
                Ok(studio) => studio,
                Err(error) => {
                    return json(
                        ErrorResponse::bad_request(format!(
                            "Couldn't find Roblox Studio: {}",
                            error
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };

            let packed_path = studio
                .content_path()
                .join(CREATE_ASSETS_DIR)
                .join(&model_path);

            if let Err(error) =
                fs::create_dir_all(&packed_path.parent().expect("no parent for packed_path"))
            {
                return json(
                    ErrorResponse::bad_request(format!(
                        "Couldn't create assets directory: {}",
                        error
                    )),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }

            let mut writer = match fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(&packed_path)
            {
                Ok(file) => file,
                Err(error) => {
                    return json(
                        ErrorResponse::bad_request(format!(
                            "Couldn't create assets file: {}",
                            error
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };

            let export_tree = WeakDom::new(
                InstanceBuilder::new("Folder")
                    .with_name("Assets")
                    .with_children(request.assets.iter().copied().map(|asset_ref| {
                        let exporting = serve_dom.get_by_ref(asset_ref).unwrap_or_else(|| {
                            unreachable!(
                                "Received asset ID for an instance not in our tree ({})",
                                asset_ref
                            )
                        });

                        // Children of instance are not used here, as Rojo has already
                        // created them, and can place them inside instead.
                        InstanceBuilder::new(&exporting.class)
                            .with_name(&exporting.name)
                            .with_properties(exporting.properties.to_owned())
                    })),
            );

            // XML is used instead of binary, as invalid XML files error,
            // while invalid binary files completely crash Studio.
            if let Err(error) =
				rbx_binary::to_writer_default(&mut writer, &export_tree, export_tree.root().children())
            {
                return json(
                    ErrorResponse::bad_request(format!("Couldn't write DOM to file: {}", error)),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }

            json_ok(&CreateAssetsResponse {
                url: format!("rbxasset://{}/{}", CREATE_ASSETS_DIR, model_path),
            })
        }))
    }
}

/// If this instance is represented by a script, try to find the correct .lua
/// file to open to edit it.
fn pick_script_path(instance: InstanceWithMeta<'_>) -> Option<PathBuf> {
    match instance.class_name() {
        "Script" | "LocalScript" | "ModuleScript" => {}
        _ => return None,
    }

    // Pick the first listed relevant path that has an extension of .lua that
    // exists.
    instance
        .metadata()
        .relevant_paths
        .iter()
        .find(|path| {
            // We should only ever open Lua files to be safe.
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("lua") => {}
                _ => return false,
            }

            fs::metadata(path)
                .map(|meta| meta.is_file())
                .unwrap_or(false)
        })
        .map(|path| path.to_owned())
}
