/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

use bytes::{Buf, BufMut, Bytes};
use http::{HeaderMap, HeaderValue};
use http::header::CONTENT_TYPE;
use hyper::{Body, StatusCode};
use hyper::body::HttpBody;
use hyper::ext::ReasonPhrase;
use mozjs::jsapi::{Heap, JSObject};
use mozjs::rust::IntoHandle;
use url::Url;

use ion::{ClassDefinition, Context, Error, ErrorKind, Local, Object, Promise, Result};
use ion::class::{NativeObject, Reflector};
use ion::typedarray::ArrayBuffer;
pub use options::*;

use crate::globals::fetch::body::FetchBody;
use crate::globals::fetch::header::HeadersKind;
use crate::globals::fetch::Headers;
use crate::promise::future_to_promise;

mod options;

#[js_class]
pub struct Response {
	reflector: Reflector,

	#[ion(no_trace)]
	pub(crate) response: Option<hyper::Response<Body>>,
	pub(crate) headers: Box<Heap<*mut JSObject>>,
	pub(crate) body: Option<FetchBody>,
	pub(crate) body_used: bool,

	pub(crate) kind: ResponseKind,
	#[ion(no_trace)]
	pub(crate) url: Option<Url>,
	pub(crate) redirected: bool,

	#[ion(no_trace)]
	pub(crate) status: Option<StatusCode>,
	pub(crate) status_text: Option<String>,

	pub(crate) range_requested: bool,
}

impl Response {
	pub fn new(response: hyper::Response<Body>, url: Url) -> Response {
		let status = response.status();
		let status_text = if let Some(reason) = response.extensions().get::<ReasonPhrase>() {
			Some(String::from_utf8(reason.as_bytes().to_vec()).unwrap())
		} else {
			status.canonical_reason().map(String::from)
		};

		Response {
			reflector: Reflector::default(),

			response: Some(response),
			headers: Box::default(),
			body: None,
			body_used: false,

			kind: ResponseKind::default(),
			url: Some(url),
			redirected: false,

			status: Some(status),
			status_text,

			range_requested: false,
		}
	}

	pub fn new_from_bytes(bytes: Bytes, url: Url) -> Response {
		let response = hyper::Response::builder().body(Body::from(bytes)).unwrap();
		Response {
			reflector: Reflector::default(),

			response: Some(response),
			headers: Box::default(),
			body: None,
			body_used: false,

			kind: ResponseKind::Basic,
			url: Some(url),
			redirected: false,

			status: Some(StatusCode::OK),
			status_text: Some(String::from("OK")),

			range_requested: false,
		}
	}
}

#[js_class]
impl Response {
	#[ion(constructor)]
	pub fn constructor(cx: &Context, body: Option<FetchBody>, init: Option<ResponseInit>) -> Result<Response> {
		let init = init.unwrap_or_default();

		let response = hyper::Response::builder().status(init.status).body(Body::empty())?;
		let mut response = Response {
			reflector: Reflector::default(),

			response: Some(response),
			headers: Box::default(),
			body: None,
			body_used: false,

			kind: ResponseKind::default(),
			url: None,
			redirected: false,

			status: Some(init.status),
			status_text: init.status_text,

			range_requested: false,
		};

		let mut headers = init.headers.into_headers(HeaderMap::new(), HeadersKind::Response)?;

		if let Some(body) = body {
			if init.status == StatusCode::NO_CONTENT
				|| init.status == StatusCode::RESET_CONTENT
				|| init.status == StatusCode::NOT_MODIFIED
			{
				return Err(Error::new(
					"Received non-null body with null body status.",
					ErrorKind::Type,
				));
			}

			if let Some(kind) = &body.kind {
				if !headers.headers.contains_key(CONTENT_TYPE) {
					headers.headers.append(CONTENT_TYPE, HeaderValue::from_str(&kind.to_string()).unwrap());
				}
			}
			response.body = Some(body);
		}

		response.headers.set(Headers::new_object(cx, Box::new(headers)));

		Ok(response)
	}

	#[ion(get)]
	pub fn get_type(&self) -> String {
		self.kind.to_string()
	}

	#[ion(get)]
	pub fn get_url(&self) -> String {
		self.url.as_ref().map(Url::to_string).unwrap_or_default()
	}

	#[ion(get)]
	pub fn get_redirected(&self) -> bool {
		self.redirected
	}

	#[ion(get)]
	pub fn get_status(&self) -> u16 {
		self.status.as_ref().map(StatusCode::as_u16).unwrap_or_default()
	}

	#[ion(get)]
	pub fn get_ok(&self) -> bool {
		self.status.as_ref().map(StatusCode::is_success).unwrap_or_default()
	}

	#[ion(get)]
	pub fn get_status_text(&self) -> String {
		self.status_text.clone().unwrap_or_default()
	}

	#[ion(get)]
	pub fn get_headers(&self) -> *mut JSObject {
		self.headers.get()
	}

	#[ion(get)]
	pub fn get_body_used(&self) -> bool {
		self.body_used
	}

	async fn read_to_bytes(&mut self) -> Result<Vec<u8>> {
		if self.body_used {
			return Err(Error::new("Response body has already been used.", None));
		}
		self.body_used = true;

		match &mut self.response {
			None => Err(Error::new("Response is a network error and cannot be read.", None)),
			Some(response) => {
				let body = response.body_mut();

				let first = if let Some(buf) = body.data().await {
					buf?
				} else {
					return Ok(Vec::new());
				};

				let second = if let Some(buf) = body.data().await {
					buf?
				} else {
					return Ok(first.to_vec());
				};

				let cap = first.remaining() + second.remaining() + body.size_hint().lower() as usize;
				let mut vec = Vec::with_capacity(cap);
				vec.put(first);
				vec.put(second);

				while let Some(buf) = body.data().await {
					vec.put(buf?);
				}

				Ok(vec)
			}
		}
	}

	#[ion(name = "arrayBuffer")]
	pub fn array_buffer<'cx>(&mut self, cx: &'cx Context) -> Option<Promise<'cx>> {
		let this = cx.root_persistent_object(self.reflector().get());
		let cx2 = unsafe { Context::new_unchecked(cx.as_ptr()) };
		let this = this.handle().into_handle();
		future_to_promise::<_, _, Error>(cx, async move {
			let mut response = Object::from(unsafe { Local::from_raw_handle(this) });
			let response = Response::get_mut_private(&mut response);
			let bytes = response.read_to_bytes().await?;
			cx2.unroot_persistent_object(this.get());
			Ok(ArrayBuffer::from(bytes))
		})
	}

	pub fn text<'cx>(&mut self, cx: &'cx Context) -> Option<Promise<'cx>> {
		let this = cx.root_persistent_object(self.reflector().get());
		let cx2 = unsafe { Context::new_unchecked(cx.as_ptr()) };
		let this = this.handle().into_handle();
		future_to_promise::<_, _, Error>(cx, async move {
			let mut response = Object::from(unsafe { Local::from_raw_handle(this) });
			let response = Response::get_mut_private(&mut response);
			let bytes = response.read_to_bytes().await?;
			cx2.unroot_persistent_object(this.get());
			String::from_utf8(bytes).map_err(|e| Error::new(&format!("Invalid UTF-8 sequence: {}", e), None))
		})
	}
}

pub fn network_error() -> Response {
	Response {
		reflector: Reflector::default(),

		response: None,
		headers: Box::default(),
		body: None,
		body_used: false,

		kind: ResponseKind::Error,
		url: None,
		redirected: false,

		status: None,
		status_text: None,

		range_requested: false,
	}
}
