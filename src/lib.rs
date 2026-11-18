// SPDX-License-Identifier: GPL-3.0-or-later
//! SigmaOS-nvme — NVMe MMIO controller, queue pairs, DMA-safe submission,
//! and completion.
//!
//! Implements NVMe 1.4 specification constructs:
//! - Submission Queue Entry (SQE) with 64-byte command format
//! - Completion Queue Entry (CQE) with 16-byte status format
//! - Admin and I/O Queue Pairs with tail/head doorbell management
//! - Block-level Read/Write/Flush command dispatch and in-memory block store
//!
//! Principles: fail-fast, no unsafe code, deterministic unit-testable logic.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvmeError {
    InvalidQueueSize(u16),
    QueueFull,
    QueueEmpty,
    QueueNotFound(u16),
    QueueAlreadyExists(u16),
    InvalidLba { lba: u64, max_lba: u64 },
    InvalidBlockCount(u32),
    CommandFailed { cid: u16, status: u16 },
    ControllerDisabled,
}

impl fmt::Display for NvmeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQueueSize(s) => write!(f, "invalid queue size: {s} (must be 2..4096)"),
            Self::QueueFull => write!(f, "submission queue is full"),
            Self::QueueEmpty => write!(f, "queue is empty"),
            Self::QueueNotFound(qid) => write!(f, "queue {qid} not found"),
            Self::QueueAlreadyExists(qid) => write!(f, "queue {qid} already exists"),
            Self::InvalidLba { lba, max_lba } => {
                write!(f, "LBA {lba} exceeds maximum addressable LBA {max_lba}")
            }
            Self::InvalidBlockCount(n) => write!(f, "invalid block count: {n}"),
            Self::CommandFailed { cid, status } => {
                write!(f, "command {cid} failed with status code 0x{status:04x}")
            }
            Self::ControllerDisabled => write!(f, "NVMe controller is disabled"),
        }
    }
}

impl std::error::Error for NvmeError {}

// ── Opcodes ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NvmeOpcode {
    Flush = 0x00,
    Write = 0x01,
    Read = 0x02,
    Identify = 0x06,
    CreateIoCq = 0x05,
    CreateIoSq = 0x09,
    DeleteIoSq = 0x08,
    DeleteIoCq = 0x04,
}

// ── Submission Queue Entry (SQE, 64-byte command format) ─────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeCommand {
    pub opcode: u8,
    pub flags: u8,
    pub cid: u16,
    pub nsid: u32,
    pub lba: u64,
    pub block_count: u32,
    pub data_payload: Vec<u8>,
}

impl NvmeCommand {
    pub fn read(cid: u16, nsid: u32, lba: u64, block_count: u32) -> Self {
        Self {
            opcode: NvmeOpcode::Read as u8,
            flags: 0,
            cid,
            nsid,
            lba,
            block_count,
            data_payload: Vec::new(),
        }
    }

    pub fn write(cid: u16, nsid: u32, lba: u64, block_count: u32, data: Vec<u8>) -> Self {
        Self {
            opcode: NvmeOpcode::Write as u8,
            flags: 0,
            cid,
            nsid,
            lba,
            block_count,
            data_payload: data,
        }
    }

    pub fn flush(cid: u16, nsid: u32) -> Self {
        Self {
            opcode: NvmeOpcode::Flush as u8,
            flags: 0,
            cid,
            nsid,
            lba: 0,
            block_count: 0,
            data_payload: Vec::new(),
        }
    }

    pub fn identify(cid: u16, nsid: u32) -> Self {
        Self {
            opcode: NvmeOpcode::Identify as u8,
            flags: 0,
            cid,
            nsid,
            lba: 0,
            block_count: 0,
            data_payload: Vec::new(),
        }
    }
}

// ── Completion Queue Entry (CQE, 16-byte completion format) ──────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeCompletion {
    pub cid: u16,
    pub sq_head: u16,
    pub sq_id: u16,
    pub status: u16,
    pub phase: bool,
    pub data_result: Vec<u8>,
}

impl NvmeCompletion {
    pub fn success(cid: u16, sq_id: u16, sq_head: u16, phase: bool) -> Self {
        Self {
            cid,
            sq_head,
            sq_id,
            status: 0, // Successful completion
            phase,
            data_result: Vec::new(),
        }
    }

    pub fn success_with_data(cid: u16, sq_id: u16, sq_head: u16, phase: bool, data: Vec<u8>) -> Self {
        Self {
            cid,
            sq_head,
            sq_id,
            status: 0,
            phase,
            data_result: data,
        }
    }

    pub fn error(cid: u16, sq_id: u16, sq_head: u16, phase: bool, status_code: u16) -> Self {
        Self {
            cid,
            sq_head,
            sq_id,
            status: status_code,
            phase,
            data_result: Vec::new(),
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == 0
    }
}

// ── Queue Pair ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct NvmeQueuePair {
    pub qid: u16,
    pub size: u16,
    sq: Vec<NvmeCommand>,
    cq: Vec<NvmeCompletion>,
    pub sq_tail: u16,
    pub sq_head: u16,
    pub cq_head: u16,
    pub cq_phase: bool,
}

impl NvmeQueuePair {
    pub fn new(qid: u16, size: u16) -> Result<Self, NvmeError> {
        if size < 2 || size > 4096 {
            return Err(NvmeError::InvalidQueueSize(size));
        }
        Ok(Self {
            qid,
            size,
            sq: Vec::with_capacity(size as usize),
            cq: Vec::with_capacity(size as usize),
            sq_tail: 0,
            sq_head: 0,
            cq_head: 0,
            cq_phase: true,
        })
    }

    pub fn submit(&mut self, cmd: NvmeCommand) -> Result<(), NvmeError> {
        if self.sq.len() >= self.size as usize {
            return Err(NvmeError::QueueFull);
        }
        self.sq.push(cmd);
        self.sq_tail = (self.sq_tail + 1) % self.size;
        Ok(())
    }

    pub fn complete(&mut self, cqe: NvmeCompletion) -> Result<(), NvmeError> {
        self.cq.push(cqe);
        self.sq_head = (self.sq_head + 1) % self.size;
        Ok(())
    }

    pub fn poll_completion(&mut self) -> Option<NvmeCompletion> {
        if self.cq.is_empty() {
            None
        } else {
            let cqe = self.cq.remove(0);
            self.cq_head = (self.cq_head + 1) % self.size;
            if self.cq_head == 0 {
                self.cq_phase = !self.cq_phase;
            }
            Some(cqe)
        }
    }

    pub fn pending_submissions(&self) -> usize {
        self.sq.len()
    }

    pub fn pending_completions(&self) -> usize {
        self.cq.len()
    }
}

// ── NVMe Controller & Namespace ──────────────────────────────────────────────

pub const BLOCK_SIZE: usize = 512;

#[derive(Debug)]
pub struct NvmeNamespace {
    pub nsid: u32,
    pub block_count: u64,
    pub block_size: usize,
    storage: HashMap<u64, [u8; BLOCK_SIZE]>,
}

impl NvmeNamespace {
    pub fn new(nsid: u32, block_count: u64) -> Self {
        Self {
            nsid,
            block_count,
            block_size: BLOCK_SIZE,
            storage: HashMap::new(),
        }
    }

    pub fn read(&self, lba: u64, count: u32) -> Result<Vec<u8>, NvmeError> {
        if lba + count as u64 > self.block_count {
            return Err(NvmeError::InvalidLba {
                lba,
                max_lba: self.block_count,
            });
        }
        let mut result = Vec::with_capacity((count as usize) * BLOCK_SIZE);
        for i in 0..count as u64 {
            let block = self.storage.get(&(lba + i)).copied().unwrap_or([0u8; BLOCK_SIZE]);
            result.extend_from_slice(&block);
        }
        Ok(result)
    }

    pub fn write(&mut self, lba: u64, count: u32, data: &[u8]) -> Result<(), NvmeError> {
        if lba + count as u64 > self.block_count {
            return Err(NvmeError::InvalidLba {
                lba,
                max_lba: self.block_count,
            });
        }
        let expected_len = (count as usize) * BLOCK_SIZE;
        if data.len() < expected_len {
            return Err(NvmeError::InvalidBlockCount(count));
        }

        for i in 0..count as u64 {
            let offset = (i as usize) * BLOCK_SIZE;
            let mut block = [0u8; BLOCK_SIZE];
            block.copy_from_slice(&data[offset..offset + BLOCK_SIZE]);
            self.storage.insert(lba + i, block);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct NvmeController {
    pub enabled: bool,
    pub model_number: String,
    pub serial_number: String,
    namespaces: HashMap<u32, NvmeNamespace>,
    queues: HashMap<u16, NvmeQueuePair>,
}

impl NvmeController {
    pub fn new(model: impl Into<String>, serial: impl Into<String>) -> Self {
        let mut ctrl = Self {
            enabled: true,
            model_number: model.into(),
            serial_number: serial.into(),
            namespaces: HashMap::new(),
            queues: HashMap::new(),
        };
        // Admin Queue Pair (QID 0) is created at controller init
        ctrl.create_queue(0, 64).unwrap();
        ctrl
    }

    pub fn add_namespace(&mut self, ns: NvmeNamespace) {
        self.namespaces.insert(ns.nsid, ns);
    }

    pub fn create_queue(&mut self, qid: u16, size: u16) -> Result<(), NvmeError> {
        if self.queues.contains_key(&qid) {
            return Err(NvmeError::QueueAlreadyExists(qid));
        }
        let qp = NvmeQueuePair::new(qid, size)?;
        self.queues.insert(qid, qp);
        Ok(())
    }

    pub fn process_command(&mut self, qid: u16, cmd: NvmeCommand) -> Result<NvmeCompletion, NvmeError> {
        if !self.enabled {
            return Err(NvmeError::ControllerDisabled);
        }

        let q = self.queues.get_mut(&qid).ok_or(NvmeError::QueueNotFound(qid))?;
        q.submit(cmd.clone())?;

        let cqe = match cmd.opcode {
            0x02 => { // Read
                if let Some(ns) = self.namespaces.get(&cmd.nsid) {
                    match ns.read(cmd.lba, cmd.block_count) {
                        Ok(data) => NvmeCompletion::success_with_data(cmd.cid, qid, q.sq_head, q.cq_phase, data),
                        Err(_) => NvmeCompletion::error(cmd.cid, qid, q.sq_head, q.cq_phase, 0x0080),
                    }
                } else {
                    NvmeCompletion::error(cmd.cid, qid, q.sq_head, q.cq_phase, 0x000B) // Invalid Namespace
                }
            }
            0x01 => { // Write
                if let Some(ns) = self.namespaces.get_mut(&cmd.nsid) {
                    match ns.write(cmd.lba, cmd.block_count, &cmd.data_payload) {
                        Ok(()) => NvmeCompletion::success(cmd.cid, qid, q.sq_head, q.cq_phase),
                        Err(_) => NvmeCompletion::error(cmd.cid, qid, q.sq_head, q.cq_phase, 0x0080),
                    }
                } else {
                    NvmeCompletion::error(cmd.cid, qid, q.sq_head, q.cq_phase, 0x000B)
                }
            }
            0x00 => { // Flush
                NvmeCompletion::success(cmd.cid, qid, q.sq_head, q.cq_phase)
            }
            0x06 => { // Identify
                let info = format!("Model={},SN={}", self.model_number, self.serial_number);
                NvmeCompletion::success_with_data(cmd.cid, qid, q.sq_head, q.cq_phase, info.into_bytes())
            }
            _ => NvmeCompletion::error(cmd.cid, qid, q.sq_head, q.cq_phase, 0x0001), // Invalid Command Opcode
        };

        q.complete(cqe.clone())?;
        Ok(cqe)
    }

    pub fn poll_queue(&mut self, qid: u16) -> Option<NvmeCompletion> {
        self.queues.get_mut(&qid).and_then(|q| q.poll_completion())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_pair_creation() {
        let qp = NvmeQueuePair::new(1, 128).unwrap();
        assert_eq!(qp.qid, 1);
        assert_eq!(qp.size, 128);
        assert_eq!(qp.pending_submissions(), 0);
        assert_eq!(qp.pending_completions(), 0);
    }

    #[test]
    fn test_invalid_queue_size() {
        assert!(NvmeQueuePair::new(1, 1).is_err());
        assert!(NvmeQueuePair::new(1, 4097).is_err());
    }

    #[test]
    fn test_controller_initialization() {
        let ctrl = NvmeController::new("SigmaOS-NVMe-Gen4", "SIGMA20261118");
        assert!(ctrl.enabled);
        assert_eq!(ctrl.model_number, "SigmaOS-NVMe-Gen4");
        assert!(ctrl.queues.contains_key(&0)); // Admin queue
    }

    #[test]
    fn test_identify_command() {
        let mut ctrl = NvmeController::new("SigmaDrive", "SN001");
        let cmd = NvmeCommand::identify(1, 0);
        let cqe = ctrl.process_command(0, cmd).unwrap();
        assert!(cqe.is_success());
        let info = String::from_utf8(cqe.data_result).unwrap();
        assert!(info.contains("SigmaDrive"));
        assert!(info.contains("SN001"));
    }

    #[test]
    fn test_read_write_namespace() {
        let mut ctrl = NvmeController::new("SigmaDrive", "SN002");
        let ns = NvmeNamespace::new(1, 2048); // 1 MB disk (2048 * 512)
        ctrl.add_namespace(ns);
        ctrl.create_queue(1, 64).unwrap(); // I/O queue

        // Write data to LBA 10
        let mut data = vec![0xAB; BLOCK_SIZE];
        data[0] = 0x42;
        data[511] = 0x99;

        let write_cmd = NvmeCommand::write(101, 1, 10, 1, data.clone());
        let write_cqe = ctrl.process_command(1, write_cmd).unwrap();
        assert!(write_cqe.is_success());

        // Read back from LBA 10
        let read_cmd = NvmeCommand::read(102, 1, 10, 1);
        let read_cqe = ctrl.process_command(1, read_cmd).unwrap();
        assert!(read_cqe.is_success());
        assert_eq!(read_cqe.data_result, data);
    }

    #[test]
    fn test_out_of_bounds_lba() {
        let mut ctrl = NvmeController::new("SigmaDrive", "SN003");
        let ns = NvmeNamespace::new(1, 10);
        ctrl.add_namespace(ns);
        ctrl.create_queue(1, 64).unwrap();

        let cmd = NvmeCommand::read(201, 1, 100, 1);
        let cqe = ctrl.process_command(1, cmd).unwrap();
        assert!(!cqe.is_success());
    }

    #[test]
    fn test_flush_command() {
        let mut ctrl = NvmeController::new("SigmaDrive", "SN004");
        let cmd = NvmeCommand::flush(301, 1);
        let cqe = ctrl.process_command(0, cmd).unwrap();
        assert!(cqe.is_success());
    }

    #[test]
    fn test_completion_polling() {
        let mut ctrl = NvmeController::new("SigmaDrive", "SN005");
        let cmd = NvmeCommand::flush(401, 1);
        let _ = ctrl.process_command(0, cmd).unwrap();

        let polled = ctrl.poll_queue(0).unwrap();
        assert_eq!(polled.cid, 401);
        assert!(ctrl.poll_queue(0).is_none());
    }
}
