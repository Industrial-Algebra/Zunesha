// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

//! Error types for Zunesha GPU device operations.
//!
//! All fallible operations return [`Result<T>`], an alias for
//! `std::result::Result<T, DeviceError>`. Errors are structured with
//! context-rich variants using `thiserror`.

use thiserror::Error;

/// Errors that can occur in GPU device operations.
///
/// Zunesha's verification posture targets *structural* device/buffer safety —
/// valid device initialisation, valid buffer allocation, valid queue
/// capability requests. Numerical correctness is not Zunesha's concern; it
/// remains Borsalino's.
#[derive(Error, Debug)]
pub enum DeviceError {
    /// No device backend is available for the current platform.
    ///
    /// Enable the `metal` feature on macOS or the `vulkan` feature on
    /// Linux/Windows once the device backends ship.
    #[error("no GPU device backend available for current platform")]
    NoDevice,

    /// Failed to initialise the GPU device.
    ///
    /// No suitable physical device was found, or the logical device could not
    /// be created with the requested capabilities.
    #[error("failed to initialise GPU device: {0}")]
    InitFailed(String),

    /// The requested queue capability is not available on the selected device.
    ///
    /// For example, requesting a graphics queue on a compute-only device
    /// (e.g. NVIDIA Grace Blackwell GB10 without a graphics-capable driver).
    #[error("requested queue capability unavailable: {message}")]
    QueueCapabilityUnavailable {
        /// Which capability was missing.
        message: String,
    },

    /// Buffer creation failed.
    ///
    /// The device could not allocate a buffer of the requested type and size,
    /// or no suitable memory type was found.
    #[error("buffer creation failed: {message}")]
    BufferCreationFailed {
        /// The platform error message.
        message: String,
    },

    /// Buffer readback failed.
    ///
    /// The buffer contents could not be mapped back to CPU memory.
    #[error("buffer readback failed: {message}")]
    BufferReadFailed {
        /// The platform error message.
        message: String,
    },

    /// Internal error — should not occur in normal operation.
    #[error("internal device error: {0}")]
    Internal(String),

    /// I/O error from the platform layer.
    #[error("platform I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type alias for Zunesha operations.
pub type Result<T> = std::result::Result<T, DeviceError>;
