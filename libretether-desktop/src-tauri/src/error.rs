//! Error type shared by the Tauri command layer. Serializes to a plain string
//! so the frontend always receives a readable message.

use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
	#[error("{0}")]
	Msg(String),
	#[error("client not found")]
	NotFound,
	#[error("client is offline")]
	Offline,
	#[error("no controller is connected")]
	NoActiveController,
	#[error("this client is already enrolled — reset its token to re-deploy")]
	AlreadyEnrolled,
	#[error("the agent did not respond in time")]
	Timeout,
	#[error("the agent returned an error: {0}")]
	Agent(String),
	#[error(transparent)]
	Io(#[from] std::io::Error),
}

impl AppError {
	pub fn msg(s: impl Into<String>) -> Self {
		Self::Msg(s.into())
	}
}

impl Serialize for AppError {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_str(&self.to_string())
	}
}

pub type AppResult<T> = Result<T, AppError>;
