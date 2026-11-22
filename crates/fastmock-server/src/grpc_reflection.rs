//! gRPC server reflection (`grpc.reflection.v1` and `v1alpha`, which share the wire format) answered from
//! the uploaded descriptor sets.

use std::collections::HashSet;

use prost::Message;
use prost_reflect::{DescriptorPool, FileDescriptor};

pub const V1: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
pub const V1ALPHA: &str = "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo";

pub fn is_reflection(path: &str) -> bool {
    path == V1 || path == V1ALPHA
}

#[derive(Clone, PartialEq, Message)]
pub struct ServerReflectionRequest {
    #[prost(string, tag = "1")]
    pub host: String,
    #[prost(oneof = "MessageRequest", tags = "3, 4, 5, 6, 7")]
    pub message_request: Option<MessageRequest>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum MessageRequest {
    #[prost(string, tag = "3")]
    FileByFilename(String),
    #[prost(string, tag = "4")]
    FileContainingSymbol(String),
    #[prost(message, tag = "5")]
    FileContainingExtension(ExtensionRequest),
    #[prost(string, tag = "6")]
    AllExtensionNumbersOfType(String),
    #[prost(string, tag = "7")]
    ListServices(String),
}

#[derive(Clone, PartialEq, Message)]
pub struct ExtensionRequest {
    #[prost(string, tag = "1")]
    pub containing_type: String,
    #[prost(int32, tag = "2")]
    pub extension_number: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServerReflectionResponse {
    #[prost(string, tag = "1")]
    pub valid_host: String,
    #[prost(message, optional, tag = "2")]
    pub original_request: Option<ServerReflectionRequest>,
    #[prost(oneof = "MessageResponse", tags = "4, 5, 6, 7")]
    pub message_response: Option<MessageResponse>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum MessageResponse {
    #[prost(message, tag = "4")]
    FileDescriptorResponse(FileDescriptorResponse),
    #[prost(message, tag = "5")]
    AllExtensionNumbersResponse(ExtensionNumberResponse),
    #[prost(message, tag = "6")]
    ListServicesResponse(ListServiceResponse),
    #[prost(message, tag = "7")]
    ErrorResponse(ErrorResponse),
}

#[derive(Clone, PartialEq, Message)]
pub struct FileDescriptorResponse {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub file_descriptor_proto: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExtensionNumberResponse {
    #[prost(string, tag = "1")]
    pub base_type_name: String,
    #[prost(int32, repeated, tag = "2")]
    pub extension_number: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ListServiceResponse {
    #[prost(message, repeated, tag = "1")]
    pub service: Vec<ServiceResponse>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServiceResponse {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ErrorResponse {
    #[prost(int32, tag = "1")]
    pub error_code: i32,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

/// The file and everything it imports, the file first.
fn with_dependencies(file: FileDescriptor) -> MessageResponse {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut todo = vec![file];
    while let Some(f) = todo.pop() {
        if !seen.insert(f.name().to_string()) {
            continue;
        }
        out.push(f.file_descriptor_proto().encode_to_vec());
        todo.extend(f.dependencies());
    }
    MessageResponse::FileDescriptorResponse(FileDescriptorResponse {
        file_descriptor_proto: out,
    })
}

fn not_found(what: String) -> MessageResponse {
    MessageResponse::ErrorResponse(ErrorResponse {
        error_code: 5,
        error_message: what,
    })
}

fn file_of_symbol(pool: &DescriptorPool, symbol: &str) -> Option<FileDescriptor> {
    let symbol = symbol.trim_start_matches('.');
    if let Some(s) = pool.get_service_by_name(symbol) {
        return Some(s.parent_file());
    }
    if let Some(m) = pool.get_message_by_name(symbol) {
        return Some(m.parent_file());
    }
    if let Some(e) = pool.get_enum_by_name(symbol) {
        return Some(e.parent_file());
    }
    if let Some(x) = pool.get_extension_by_name(symbol) {
        return Some(x.parent_file());
    }
    let (parent, member) = symbol.rsplit_once('.')?;
    if let Some(s) = pool.get_service_by_name(parent)
        && s.methods().any(|m| m.name() == member)
    {
        return Some(s.parent_file());
    }
    if let Some(m) = pool.get_message_by_name(parent)
        && (m.get_field_by_name(member).is_some() || m.oneofs().any(|o| o.name() == member))
    {
        return Some(m.parent_file());
    }
    if let Some(e) = pool.get_enum_by_name(parent)
        && e.get_value_by_name(member).is_some()
    {
        return Some(e.parent_file());
    }
    None
}

/// Answers one reflection request.
pub fn answer(pool: &DescriptorPool, req: ServerReflectionRequest) -> ServerReflectionResponse {
    let response = match &req.message_request {
        None => MessageResponse::ErrorResponse(ErrorResponse {
            error_code: 3,
            error_message: "empty reflection request".into(),
        }),
        Some(MessageRequest::ListServices(_)) => {
            let mut names: Vec<String> =
                pool.services().map(|s| s.full_name().to_string()).collect();
            names.sort();
            MessageResponse::ListServicesResponse(ListServiceResponse {
                service: names
                    .into_iter()
                    .map(|name| ServiceResponse { name })
                    .collect(),
            })
        }
        Some(MessageRequest::FileByFilename(name)) => match pool.get_file_by_name(name) {
            Some(f) => with_dependencies(f),
            None => not_found(format!("file {name} not found")),
        },
        Some(MessageRequest::FileContainingSymbol(symbol)) => match file_of_symbol(pool, symbol) {
            Some(f) => with_dependencies(f),
            None => not_found(format!("symbol {symbol} not found")),
        },
        Some(MessageRequest::FileContainingExtension(x)) => {
            match pool
                .get_message_by_name(x.containing_type.trim_start_matches('.'))
                .and_then(|m| m.get_extension(x.extension_number as u32))
            {
                Some(e) => with_dependencies(e.parent_file()),
                None => not_found(format!(
                    "extension {} of {} not found",
                    x.extension_number, x.containing_type
                )),
            }
        }
        Some(MessageRequest::AllExtensionNumbersOfType(t)) => {
            match pool.get_message_by_name(t.trim_start_matches('.')) {
                Some(m) => MessageResponse::AllExtensionNumbersResponse(ExtensionNumberResponse {
                    base_type_name: m.full_name().to_string(),
                    extension_number: m.extensions().map(|e| e.number() as i32).collect(),
                }),
                None => not_found(format!("type {t} not found")),
            }
        }
    };
    ServerReflectionResponse {
        valid_host: req.host.clone(),
        original_request: Some(req),
        message_response: Some(response),
    }
}
