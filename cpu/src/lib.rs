// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
//
// Copyright (c) 2023 China Telecom Co.,Ltd. All rights reserved.
// 
// Modifications made by China Telecom Co.,Ltd:
// - Modify cpu module for risc-v architecture
//
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

#[cfg(target_arch = "riscv64")]
mod riscv;

pub mod error;
use anyhow::{anyhow, Context, Result};
pub use error::CpuError;
use machine_manager::qmp::qmp_schema;
#[cfg(target_arch = "riscv64")]
pub use riscv::RISCVCPUBootConfig as CPUBootConfig;
#[cfg(target_arch = "riscv64")]
pub use riscv::RISCVCPUCaps as CPUCaps;
#[cfg(target_arch = "riscv64")]
pub use riscv::RISCVCPUState as ArchCPU;
#[cfg(target_arch = "riscv64")]
pub use riscv::RISCVCPUTopology as CPUTopology;

use std::cell::RefCell;
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, Weak};
use std::thread;
use std::time::Duration;

use kvm_ioctls::{VcpuExit, VcpuFd};
use libc::{c_int, c_void, siginfo_t};
use log::{error, info, warn};
use machine_manager::event;
use machine_manager::machine::MachineInterface;
use machine_manager::{qmp::qmp_schema as schema, qmp::QmpChannel};
use vmm_sys_util::signal::{register_signal_handler, Killable};

// SIGRTMIN = 34 (GNU, in MUSL is 35) and SIGRTMAX = 64  in linux, VCPU signal
// number should be assigned to SIGRTMIN + n, (n = 0...30).
#[cfg(not(target_env = "musl"))]
const VCPU_TASK_SIGNAL: i32 = 34;
#[cfg(target_env = "musl")]
const VCPU_TASK_SIGNAL: i32 = 35;
#[cfg(not(target_env = "musl"))]
const VCPU_RESET_SIGNAL: i32 = 35;
#[cfg(target_env = "musl")]
const VCPU_RESET_SIGNAL: i32 = 36;

/// The boot start value can be verified before kernel start.
#[cfg(feature = "boot_time")]
const MAGIC_VALUE_SIGNAL_GUEST_BOOT_START: u8 = 0x01;
/// The boot complete value can be verified before init guest userspace.
#[cfg(feature = "boot_time")]
const MAGIC_VALUE_SIGNAL_GUEST_BOOT_COMPLETE: u8 = 0x02;

/// State for `CPU` lifecycle.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CpuLifecycleState {
    /// `CPU` structure is only be initialized, but nothing set.
    Nothing = 0,
    /// `CPU` structure's property is set with configuration.
    Created = 1,
    /// `CPU` start to be running.
    Running = 2,
    /// `CPU` thread is sleeping.
    Paused = 3,
    /// `CPU` structure is going to destroy.
    Stopping = 4,
    /// `CPU` structure destroyed, will be dropped soon.
    Stopped = 5,
}

/// Trait to handle `CPU` lifetime.
#[allow(clippy::upper_case_acronyms)]
pub trait CPUInterface {
    /// Realize `CPU` structure, set registers value for `CPU`.
    fn realize(
        &self,
        boot: &CPUBootConfig,
        topology: &CPUTopology,
    ) -> Result<()>;

    /// Start `CPU` thread and run virtual CPU in kvm.
    ///
    /// # Arguments
    ///
    /// * `cpu` - The cpu instance shared in thread.
    /// * `thread_barrier` - The cpu thread barrier.
    /// * `paused` - After started, paused vcpu or not.
    fn start(cpu: Arc<Self>, thread_barrier: Arc<Barrier>, paused: bool) -> Result<()>
    where
        Self: std::marker::Sized;

    /// Kick `CPU` to exit kvm emulation.
    fn kick(&self) -> Result<()>;

    /// Make `CPU` lifecycle from `Running` to `Paused`.
    fn pause(&self) -> Result<()>;

    /// Make `CPU` lifecycle from `Paused` to `Running`.
    fn resume(&self) -> Result<()>;

    /// Make `CPU` lifecycle to `Stopping`, then `Stopped`.
    fn destroy(&self) -> Result<()>;

    /// Reset registers value for `CPU`.
    fn reset(&self) -> Result<()>;

    /// Make `CPU` destroy because of guest inner shutdown.
    fn guest_shutdown(&self) -> Result<()>;

    /// Make `CPU` destroy because of guest inner reset.
    fn guest_reset(&self) -> Result<()>;

    /// Handle vcpu event from `kvm`.
    fn kvm_vcpu_exec(&self) -> Result<bool>;
}

/// `CPU` is a wrapper around creating and using a kvm-based VCPU.
#[allow(clippy::upper_case_acronyms)]
pub struct CPU {
    /// ID of this virtual CPU, `0` means this cpu is primary `CPU`.
    id: u8,
    /// The file descriptor of this kvm-based VCPU.
    fd: Arc<VcpuFd>,
    /// Architecture special CPU property.
    arch_cpu: Arc<Mutex<ArchCPU>>,
    /// LifeCycle state of kvm-based VCPU.
    state: Arc<(Mutex<CpuLifecycleState>, Condvar)>,
    /// The thread handler of this virtual CPU.
    task: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    /// The thread tid of this VCPU.
    tid: Arc<Mutex<Option<u64>>>,
    /// The VM combined by this VCPU.
    vm: Weak<Mutex<dyn MachineInterface + Send + Sync>>,
    /// The capability of VCPU.
    caps: CPUCaps,
    /// The state backup of architecture CPU right before boot.
    boot_state: Arc<Mutex<ArchCPU>>,
    /// Sync the pause state of vCPU in kvm and userspace.
    pause_signal: Arc<AtomicBool>,
}

impl CPU {
    /// Allocates a new `CPU` for `vm`
    ///
    /// # Arguments
    ///
    /// * `vcpu_fd` - The file descriptor of this `CPU`.
    /// * `id` - ID of this `CPU`.
    /// * `arch_cpu` - Architecture special `CPU` property.
    /// * `vm` - The virtual machine this `CPU` gets attached to.
    pub fn new(
        vcpu_fd: Arc<VcpuFd>,
        id: u8,
        arch_cpu: Arc<Mutex<ArchCPU>>,
        vm: Arc<Mutex<dyn MachineInterface + Send + Sync>>,
    ) -> Self {
        CPU {
            id,
            fd: vcpu_fd,
            arch_cpu,
            state: Arc::new((Mutex::new(CpuLifecycleState::Created), Condvar::new())),
            task: Arc::new(Mutex::new(None)),
            tid: Arc::new(Mutex::new(None)),
            vm: Arc::downgrade(&vm),
            caps: CPUCaps::init_capabilities(),
            boot_state: Arc::new(Mutex::new(ArchCPU::default())),
            pause_signal: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_to_boot_state(&self) {
        self.arch_cpu.lock().unwrap().set(&self.boot_state);
    }

    /// Get this `CPU`'s ID.
    pub fn id(&self) -> u8 {
        self.id
    }

    /// Get this `CPU`'s file descriptor.
    pub fn fd(&self) -> &Arc<VcpuFd> {
        &self.fd
    }

    /// Get this `CPU`'s state.
    pub fn state(&self) -> &(Mutex<CpuLifecycleState>, Condvar) {
        self.state.as_ref()
    }

    /// Get this `CPU`'s architecture-special property.
    pub fn arch(&self) -> &Arc<Mutex<ArchCPU>> {
        &self.arch_cpu
    }

    /// Set task the `CPU` to handle.
    fn set_task(&self, task: Option<thread::JoinHandle<()>>) {
        let mut data = self.task.lock().unwrap();
        (*data).take().map(thread::JoinHandle::join);
        *data = task;
    }

    /// Get this `CPU`'s thread id.
    pub fn tid(&self) -> u64 {
        (*self.tid.lock().unwrap()).unwrap_or(0)
    }

    /// Set thread id for `CPU`.
    fn set_tid(&self) {
        *self.tid.lock().unwrap() = Some(util::unix::gettid());
    }
}

impl CPUInterface for CPU {
    fn realize(
        &self,
        boot: &CPUBootConfig,
        topology: &CPUTopology,
        #[cfg(target_arch = "aarch64")] config: &CPUFeatures,
    ) -> Result<()> {
        trace_cpu_boot_config(boot);
        let (cpu_state, _) = &*self.state;
        if *cpu_state.lock().unwrap() != CpuLifecycleState::Created {
            return Err(anyhow!(CpuError::RealizeVcpu(format!(
                "VCPU{} may has realized.",
                self.id()
            ))));
        }

        self.arch_cpu
            .lock()
            .unwrap()
            .set_boot_config(
                &self.fd,
                boot,
                #[cfg(target_arch = "aarch64")]
                config,
            )
            .with_context(|| "Failed to realize arch cpu")?;

        self.arch_cpu
            .lock()
            .unwrap()
            .set_cpu_topology(topology)
            .with_context(|| "Failed to realize arch cpu")?;

        self.boot_state.lock().unwrap().set(&self.arch_cpu);
        Ok(())
    }

    fn resume(&self) -> Result<()> {
        let (cpu_state_locked, cvar) = &*self.state;
        let mut cpu_state = cpu_state_locked.lock().unwrap();
        if *cpu_state == CpuLifecycleState::Running {
            warn!("vcpu{} in running state, no need to resume", self.id());
            return Ok(());
        }

        *cpu_state = CpuLifecycleState::Running;
        self.pause_signal.store(false, Ordering::SeqCst);
        drop(cpu_state);
        cvar.notify_one();
        Ok(())
    }

    fn start(cpu: Arc<CPU>, thread_barrier: Arc<Barrier>, paused: bool) -> Result<()> {
        let (cpu_state, _) = &*cpu.state;
        if *cpu_state.lock().unwrap() == CpuLifecycleState::Running {
            return Err(anyhow!(CpuError::StartVcpu(
                "Cpu is already running".to_string()
            )));
        }
        if paused {
            *cpu_state.lock().unwrap() = CpuLifecycleState::Paused;
        } else {
            *cpu_state.lock().unwrap() = CpuLifecycleState::Running;
        }

        let local_cpu = cpu.clone();
        let cpu_thread_worker = CPUThreadWorker::new(cpu);
        let handle = thread::Builder::new()
            .name(format!("CPU {}/KVM", local_cpu.id))
            .spawn(move || {
                if let Err(e) = cpu_thread_worker.handle(thread_barrier) {
                    error!(
                        "{}",
                        format!(
                            "Some error occurred in cpu{} thread: {:?}",
                            cpu_thread_worker.thread_cpu.id, e
                        )
                    );
                }
            })
            .with_context(|| format!("Failed to create thread for CPU {}/KVM", local_cpu.id()))?;
        local_cpu.set_task(Some(handle));
        Ok(())
    }

    fn reset(&self) -> Result<()> {
        let task = self.task.lock().unwrap();
        match task.as_ref() {
            Some(thread) => thread
                .kill(VCPU_RESET_SIGNAL)
                .with_context(|| anyhow!(CpuError::KickVcpu("Fail to reset vcpu".to_string()))),
            None => {
                warn!("VCPU thread not started, no need to reset");
                Ok(())
            }
        }
    }

    fn kick(&self) -> Result<()> {
        let task = self.task.lock().unwrap();
        match task.as_ref() {
            Some(thread) => thread
                .kill(VCPU_TASK_SIGNAL)
                .with_context(|| anyhow!(CpuError::KickVcpu("Fail to kick vcpu".to_string()))),
            None => {
                warn!("VCPU thread not started, no need to kick");
                Ok(())
            }
        }
    }

    fn pause(&self) -> Result<()> {
        let task = self.task.lock().unwrap();
        let (cpu_state, cvar) = &*self.state;

        if *cpu_state.lock().unwrap() == CpuLifecycleState::Running {
            *cpu_state.lock().unwrap() = CpuLifecycleState::Paused;
            cvar.notify_one()
        }

        match task.as_ref() {
            Some(thread) => {
                if let Err(e) = thread.kill(VCPU_TASK_SIGNAL) {
                    return Err(anyhow!(CpuError::StopVcpu(format!("{:?}", e))));
                }
            }
            None => {
                warn!("vCPU thread not started, no need to stop");
                return Ok(());
            }
        }

        // It shall wait for the vCPU pause state from kvm exits.
        loop {
            if self.pause_signal.load(Ordering::SeqCst) {
                break;
            }
        }

        Ok(())
    }

    fn destroy(&self) -> Result<()> {
        let (cpu_state, cvar) = &*self.state;
        if *cpu_state.lock().unwrap() == CpuLifecycleState::Running {
            *cpu_state.lock().unwrap() = CpuLifecycleState::Stopping;
        } else if *cpu_state.lock().unwrap() == CpuLifecycleState::Stopped {
            *cpu_state.lock().unwrap() = CpuLifecycleState::Nothing;
            return Ok(());
        }

        self.kick()?;
        let mut cpu_state = cpu_state.lock().unwrap();
        cpu_state = cvar
            .wait_timeout(cpu_state, Duration::from_millis(32))
            .unwrap()
            .0;

        if *cpu_state == CpuLifecycleState::Stopped {
            *cpu_state = CpuLifecycleState::Nothing;
            Ok(())
        } else {
            Err(anyhow!(CpuError::DestroyVcpu(format!(
                "VCPU still in {:?} state",
                *cpu_state
            ))))
        }
    }

    fn guest_shutdown(&self) -> Result<()> {
        let (cpu_state, _) = &*self.state;
        *cpu_state.lock().unwrap() = CpuLifecycleState::Stopped;

        if let Some(vm) = self.vm.upgrade() {
            vm.lock().unwrap().destroy();
        } else {
            return Err(anyhow!(CpuError::NoMachineInterface));
        }

        if QmpChannel::is_connected() {
            let shutdown_msg = schema::Shutdown {
                guest: true,
                reason: "guest-shutdown".to_string(),
            };
            event!(Shutdown; shutdown_msg);
        }

        Ok(())
    }

    fn guest_reset(&self) -> Result<()> {
        if let Some(vm) = self.vm.upgrade() {
            vm.lock().unwrap().reset();
        } else {
            return Err(anyhow!(CpuError::NoMachineInterface));
        }

        if QmpChannel::is_connected() {
            let reset_msg = schema::Reset { guest: true };
            event!(Reset; reset_msg);
        }

        Ok(())
    }

    fn kvm_vcpu_exec(&self) -> Result<bool> {
        let vm = if let Some(vm) = self.vm.upgrade() {
            vm
        } else {
            return Err(anyhow!(CpuError::NoMachineInterface));
        };

        match self.fd.run() {
            Ok(run) => match run {
                VcpuExit::MmioRead(addr, data) => {
                    vm.lock().unwrap().mmio_read(addr, data);
                }
                VcpuExit::MmioWrite(addr, data) => {
                    #[cfg(all(target_arch = "aarch64", feature = "boot_time"))]
                    capture_boot_signal(addr, data);

                    vm.lock().unwrap().mmio_write(addr, data);
                }
                VcpuExit::SystemEvent(event, flags) => {
                    if event == kvm_bindings::KVM_SYSTEM_EVENT_SHUTDOWN {
                        info!(
                            "Vcpu{} received an KVM_SYSTEM_EVENT_SHUTDOWN signal",
                            self.id()
                        );
                        self.guest_shutdown()
                            .with_context(|| "Some error occurred in guest shutdown")?;
                    } else if event == kvm_bindings::KVM_SYSTEM_EVENT_RESET {
                        info!(
                            "Vcpu{} received an KVM_SYSTEM_EVENT_RESET signal",
                            self.id()
                        );
                        self.guest_reset()
                            .with_context(|| "Some error occurred in guest reset")?;
                        return Ok(true);
                    } else {
                        error!(
                            "Vcpu{} received unexpected system event with type 0x{:x}, flags 0x{:x}",
                            self.id(),
                            event,
                            flags
                        );
                    }

                    return Ok(false);
                }
                VcpuExit::FailEntry(reason, cpuid) => {
                    info!(
                        "Vcpu{} received KVM_EXIT_FAIL_ENTRY signal. the vcpu could not be run due to unknown reasons({})",
                        cpuid, reason
                    );
                    return Ok(false);
                }
                VcpuExit::InternalError => {
                    info!("Vcpu{} received KVM_EXIT_INTERNAL_ERROR signal", self.id());
                    return Ok(false);
                }
                r => {
                    return Err(anyhow!(CpuError::VcpuExitReason(
                        self.id(),
                        format!("{:?}", r)
                    )));
                }
            },
            Err(ref e) => {
                match e.errno() {
                    libc::EAGAIN => {}
                    libc::EINTR => {
                        self.fd.set_kvm_immediate_exit(0);
                    }
                    _ => {
                        return Err(anyhow!(CpuError::UnhandledKvmExit(self.id())));
                    }
                };
            }
        }
        Ok(true)
    }
}

/// The struct to handle events in cpu thread.
#[allow(clippy::upper_case_acronyms)]
struct CPUThreadWorker {
    thread_cpu: Arc<CPU>,
}

impl CPUThreadWorker {
    thread_local!(static LOCAL_THREAD_VCPU: RefCell<Option<CPUThreadWorker>> = RefCell::new(None));

    /// Allocates a new `CPUThreadWorker`.
    fn new(thread_cpu: Arc<CPU>) -> Self {
        CPUThreadWorker { thread_cpu }
    }

    /// Init vcpu thread static variable.
    fn init_local_thread_vcpu(&self) {
        Self::LOCAL_THREAD_VCPU.with(|thread_vcpu| {
            *thread_vcpu.borrow_mut() = Some(CPUThreadWorker {
                thread_cpu: self.thread_cpu.clone(),
            });
        })
    }

    fn run_on_local_thread_vcpu<F>(func: F) -> Result<()>
    where
        F: FnOnce(&CPU),
    {
        Self::LOCAL_THREAD_VCPU.with(|thread_vcpu| {
            if let Some(local_thread_vcpu) = thread_vcpu.borrow().as_ref() {
                func(&local_thread_vcpu.thread_cpu);
                Ok(())
            } else {
                Err(anyhow!(CpuError::VcpuLocalThreadNotPresent))
            }
        })
    }

    /// Init signal for `CPU` event.
    fn init_signals() -> Result<()> {
        extern "C" fn handle_signal(signum: c_int, _: *mut siginfo_t, _: *mut c_void) {
            match signum {
                VCPU_TASK_SIGNAL => {
                    let _ = CPUThreadWorker::run_on_local_thread_vcpu(|vcpu| {
                        vcpu.fd().set_kvm_immediate_exit(1);
                        // Setting pause_signal to be `true` if kvm changes vCPU to pause state.
                        vcpu.pause_signal.store(true, Ordering::SeqCst);
                        fence(Ordering::Release)
                    });
                }
                VCPU_RESET_SIGNAL => {
                    let _ = CPUThreadWorker::run_on_local_thread_vcpu(|vcpu| {
                        if let Err(e) = vcpu.arch_cpu.lock().unwrap().reset_vcpu(
                            &vcpu.fd,
                        ) {
                            error!("Failed to reset vcpu state: {}", e.to_string())
                        }
                    });
                }
                _ => {}
            }
        }

        register_signal_handler(VCPU_TASK_SIGNAL, handle_signal)
            .with_context(|| "Failed to register VCPU_TASK_SIGNAL signal.")?;
        register_signal_handler(VCPU_RESET_SIGNAL, handle_signal)
            .with_context(|| "Failed to register VCPU_TASK_SIGNAL signal.")?;

        Ok(())
    }

    /// Judge whether the kvm vcpu is ready to emulate.
    fn ready_for_running(&self) -> Result<bool> {
        let mut flag = 0_u32;
        let (cpu_state_locked, cvar) = &*self.thread_cpu.state;
        let mut cpu_state = cpu_state_locked.lock().unwrap();

        loop {
            match *cpu_state {
                CpuLifecycleState::Paused => {
                    if flag == 0 {
                        info!("Vcpu{} paused", self.thread_cpu.id);
                        flag = 1;
                    }
                    cpu_state = cvar.wait(cpu_state).unwrap();
                }
                CpuLifecycleState::Running => {
                    return Ok(true);
                }
                CpuLifecycleState::Stopping | CpuLifecycleState::Stopped => {
                    info!("Vcpu{} shutdown", self.thread_cpu.id);
                    return Ok(false);
                }
                _ => {
                    warn!("Unknown Vmstate");
                    return Ok(true);
                }
            }
        }
    }

    /// Handle the all events in vcpu thread.
    fn handle(&self, thread_barrier: Arc<Barrier>) -> Result<()> {
        self.init_local_thread_vcpu();
        if let Err(e) = Self::init_signals() {
            error!(
                "{}",
                format!("Failed to init cpu{} signal:{:?}", self.thread_cpu.id, e)
            );
        }

        self.thread_cpu.set_tid();

        // The vcpu thread is going to run,
        // reset its running environment.
        #[cfg(not(test))]
        self.thread_cpu
            .reset()
            .with_context(|| "Failed to reset for cpu register state")?;

        // Wait for all vcpu to complete the running
        // environment initialization.
        thread_barrier.wait();

        info!("vcpu{} start running", self.thread_cpu.id);
        while let Ok(true) = self.ready_for_running() {
            #[cfg(not(test))]
            if !self
                .thread_cpu
                .kvm_vcpu_exec()
                .with_context(|| format!("VCPU {}/KVM emulate error!", self.thread_cpu.id()))?
            {
                break;
            }

            #[cfg(test)]
            {
                thread::sleep(Duration::from_millis(5));
            }
        }

        // The vcpu thread is about to exit, marking the state
        // of the CPU state as Stopped.
        let (cpu_state, cvar) = &*self.thread_cpu.state;
        *cpu_state.lock().unwrap() = CpuLifecycleState::Stopped;
        cvar.notify_one();

        Ok(())
    }
}

/// The wrapper for topology for VCPU.
#[derive(Clone)]
pub struct CpuTopology {
    /// Number of vcpus in VM.
    pub nrcpus: u8,
    /// Number of sockets in VM.
    pub sockets: u8,
    /// Number of dies in one socket.
    pub dies: u8,
    /// Number of clusters in one die.
    pub clusters: u8,
    /// Number of cores in one cluster.
    pub cores: u8,
    /// Number of threads in one core.
    pub threads: u8,
    /// Number of online vcpus in VM.
    pub max_cpus: u8,
    /// Online mask number of all vcpus.
    pub online_mask: Arc<Mutex<Vec<u8>>>,
}

impl CpuTopology {
    /// * `nr_cpus`: Number of vcpus in one VM.
    /// * `nr_sockets`: Number of sockets in one VM.
    /// * `nr_dies`: Number of dies in one socket.
    /// * `nr_clusters`: Number of clusters in one die.
    /// * `nr_cores`: Number of cores in one cluster.
    /// * `nr_threads`: Number of threads in one core.
    /// * `max_cpus`: Number of online vcpus in VM.
    pub fn new(
        nr_cpus: u8,
        nr_sockets: u8,
        nr_dies: u8,
        nr_clusters: u8,
        nr_cores: u8,
        nr_threads: u8,
        max_cpus: u8,
    ) -> Self {
        let mut mask: Vec<u8> = vec![0; max_cpus as usize];
        (0..nr_cpus as usize).for_each(|index| {
            mask[index] = 1;
        });
        Self {
            nrcpus: nr_cpus,
            sockets: nr_sockets,
            dies: nr_dies,
            clusters: nr_clusters,
            cores: nr_cores,
            threads: nr_threads,
            max_cpus,
            online_mask: Arc::new(Mutex::new(mask)),
        }
    }

    /// Get online mask for a cpu.
    ///
    /// # Notes
    ///
    /// When `online_mask` is `0`, vcpu is offline. When `online_mask` is `1`,
    /// vcpu is online.
    ///
    /// # Arguments
    ///
    /// * `vcpu_id` - ID of vcpu.
    pub fn get_mask(&self, vcpu_id: usize) -> u8 {
        let mask = self.online_mask.lock().unwrap();
        mask[vcpu_id]
    }

    /// Get single cpu topology for vcpu, return this vcpu's `socket-id`,
    /// `core-id` and `thread-id`.
    ///
    /// # Arguments
    ///
    /// * `vcpu_id` - ID of vcpu.
    fn get_topo_item(&self, vcpu_id: usize) -> (u8, u8, u8, u8, u8) {
        let socketid: u8 = vcpu_id as u8 / (self.dies * self.clusters * self.cores * self.threads);
        let dieid: u8 = (vcpu_id as u8 / (self.clusters * self.cores * self.threads)) % self.dies;
        let clusterid: u8 = (vcpu_id as u8 / (self.cores * self.threads)) % self.clusters;
        let coreid: u8 = (vcpu_id as u8 / self.threads) % self.cores;
        let threadid: u8 = vcpu_id as u8 % self.threads;
        (socketid, dieid, clusterid, coreid, threadid)
    }

    pub fn get_topo_instance_for_qmp(&self, cpu_index: usize) -> qmp_schema::CpuInstanceProperties {
        let (socketid, _dieid, _clusterid, coreid, threadid) = self.get_topo_item(cpu_index);
        qmp_schema::CpuInstanceProperties {
            node_id: None,
            socket_id: Some(socketid as isize),
            core_id: Some(coreid as isize),
            thread_id: Some(threadid as isize),
        }
    }
}

fn trace_cpu_boot_config(cpu_boot_config: &CPUBootConfig) {
    util::ftrace!(trace_CPU_boot_config, "{:#?}", cpu_boot_config);
}

/// Capture the boot signal that trap from guest kernel, and then record
/// kernel boot timestamp.
#[cfg(feature = "boot_time")]
fn capture_boot_signal(addr: u64, data: &[u8]) {
    if addr == MAGIC_SIGNAL_GUEST_BOOT {
        if data[0] == MAGIC_VALUE_SIGNAL_GUEST_BOOT_START {
            info!("Kernel starts to boot!");
        } else if data[0] == MAGIC_VALUE_SIGNAL_GUEST_BOOT_COMPLETE {
            info!("Kernel boot complete!");
        }
    }
}
