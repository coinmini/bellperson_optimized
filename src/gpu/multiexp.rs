use super::error::{GPUError, GPUResult};
use super::locks;
use super::sources;
use super::utils;
use crate::bls::Engine;
use crate::multicore::Worker;
use crate::multiexp::{multiexp as cpu_multiexp, multiexp_with_cpu, FullDensity};
use ff::{PrimeField, ScalarEngine};
use groupy::{CurveAffine, CurveProjective};
use log::{error, info};
use rust_gpu_tools::*;
use std::any::TypeId;
use std::sync::Arc;
// use std::time::Instant;

use std::sync::mpsc;
extern crate scoped_threadpool;
use scoped_threadpool::Pool;
use crate::SynthesisError;

// const MAX_WINDOW_SIZE: usize = 11;
const LOCAL_WORK_SIZE: usize = 256;
// const MEMORY_PADDING: f64 = 0.1f64; // Let 20% of GPU memory be free

pub fn get_cpu_utilization() -> f64 {
    use std::env;
    env::var("BELLMAN_CPU_UTILIZATION")
        .and_then(|v| match v.parse() {
            Ok(val) => Ok(val),
            Err(_) => {
                error!("Invalid BELLMAN_CPU_UTILIZATION! Defaulting to 0...");
                Ok(0f64)
            }
        })
        .unwrap_or(0f64)
        .max(0f64)
        .min(1f64)
}

// Multiexp kernel for a single GPU
pub struct SingleMultiexpKernel<E>
where
    E: Engine,
{
    program: opencl::Program,

    core_count: usize,
    n: usize,

    priority: bool,
    _phantom: std::marker::PhantomData<E::Fr>,
}

fn calc_num_groups(core_count: usize, num_windows: usize) -> usize {
    // Observations show that we get the best performance when num_groups * num_windows ~= 2 * CUDA_CORES
    2 * core_count / num_windows
}

// fn calc_window_size(n: usize, exp_bits: usize, core_count: usize) -> usize {
//     // window_size = ln(n / num_groups)
//     // num_windows = exp_bits / window_size
//     // num_groups = 2 * core_count / num_windows = 2 * core_count * window_size / exp_bits
//     // window_size = ln(n / num_groups) = ln(n * exp_bits / (2 * core_count * window_size))
//     // window_size = ln(exp_bits * n / (2 * core_count)) - ln(window_size)
//     //
//     // Thus we need to solve the following equation:
//     // window_size + ln(window_size) = ln(exp_bits * n / (2 * core_count))
//     let lower_bound = (((exp_bits * n) as f64) / ((4 * core_count) as f64)).ln();
//     for w in 0..MAX_WINDOW_SIZE {
//         if (w as f64) + (w as f64).ln() > lower_bound {
//             return w;
//         }
//     }

//     MAX_WINDOW_SIZE
// }

// fn calc_best_chunk_size(max_window_size: usize, core_count: usize, exp_bits: usize) -> usize {
//     // Best chunk-size (N) can also be calculated using the same logic as calc_window_size:
//     // n = e^window_size * window_size * 2 * core_count / exp_bits
//     (((max_window_size as f64).exp() as f64)
//         * (max_window_size as f64)
//         * 4f64
//         * (core_count as f64)
//         / (exp_bits as f64))
//         .ceil() as usize
// }

// fn calc_chunk_size<E>(mem: u64, core_count: usize) -> usize
// where
//     E: Engine,
// {
//     let aff_size = std::mem::size_of::<E::G1Affine>() + std::mem::size_of::<E::G2Affine>();
//     let exp_size = exp_size::<E>();
//     let proj_size = std::mem::size_of::<E::G1>() + std::mem::size_of::<E::G2>();
//     ((((mem as f64) * (1f64 - MEMORY_PADDING)) as usize)
//         - (4 * core_count * ((1 << MAX_WINDOW_SIZE) + 1) * proj_size))
//         / (aff_size + exp_size)
// }

fn exp_size<E: Engine>() -> usize {
    std::mem::size_of::<<E::Fr as ff::PrimeField>::Repr>()
}

// 单卡单任务
impl<E> SingleMultiexpKernel<E>
where
    E: Engine,
{
    pub fn create(d: opencl::Device, priority: bool) -> GPUResult<SingleMultiexpKernel<E>> {
        let src = sources::kernel::<E>(d.brand() == opencl::Brand::Nvidia);

        // let exp_bits = exp_size::<E>() * 8;
        let core_count = utils::get_core_count(&d);
        // let mem = d.memory();
        // let max_n = calc_chunk_size::<E>(mem, core_count);
        // let best_n = calc_best_chunk_size(MAX_WINDOW_SIZE, core_count, exp_bits);
        // let n = std::cmp::min(max_n, best_n);
        //这里的n数据不会真正去用
        let n = 33554466;

        Ok(SingleMultiexpKernel {
            program: opencl::Program::from_opencl(d, &src)?,
            core_count,
            n,
            priority,
            _phantom: std::marker::PhantomData,
        })
    }

    pub fn multiexp<G>(
        &mut self,
        bases: &[G],
        exps: &[<<G::Engine as ScalarEngine>::Fr as PrimeField>::Repr],
        n: usize,
        jack_windows_size: usize,
    ) -> GPUResult<<G as CurveAffine>::Projective>
    where
        G: CurveAffine,
    {
        if locks::PriorityLock::should_break(self.priority) {
            return Err(GPUError::GPUTaken);
        }

        let exp_bits = exp_size::<E>() * 8;
        // let window_size = calc_window_size(n as usize, exp_bits, self.core_count);
        let window_size = jack_windows_size;
        let num_windows = ((exp_bits as f64) / (jack_windows_size as f64)).ceil() as usize;
        let num_groups = calc_num_groups(self.core_count, num_windows);
        let bucket_len = 1 << jack_windows_size;

        info!("bucket_len is :{}",  bucket_len);

        // let size1 = std::mem::size_of::<G>();
        // let size2 = std::mem::size_of::<<<G::Engine as ScalarEngine>::Fr as PrimeField>::Repr>();
        // let size3 = std::mem::size_of::<<G as CurveAffine>::Projective>();
        // let mem1 = size1 * n;
        // let mem2 = size2 * n;
        // let mem3 = size3 * 4 * self.core_count * bucket_len;
        // let mem4 = size3 * 4 * self.core_count;
        // info!("ZQ: GPU mem need: {}Mbyte", (mem1 + mem2 + mem3 + mem4)/(1024*1024));

        // Each group will have `num_windows` threads and as there are `num_groups` groups, there will
        // be `num_groups` * `num_windows` threads in total.
        // Each thread will use `num_groups` * `num_windows` * `bucket_len` buckets.
        let size1 = std::mem::size_of::<G>();
        let size2 = std::mem::size_of::<<<G::Engine as ScalarEngine>::Fr as PrimeField>::Repr>();
        let size3 = std::mem::size_of::<<G as CurveAffine>::Projective>();
        let mem1 = size1 * n;
        let mem2 = size2 * n;
        let mem3 = size3 * 2 * self.core_count * bucket_len;
        let mem4 = size3 * 2 * self.core_count;
        info!("GABEDEBUG: <G> size:{}, <PrimeField> size:{}, <Projective> size:{}", size1, size2, size3);
        info!("GABEDEBUG: GPU mem need:{}byte, {}Mbyte", mem1 + mem2 + mem3 + mem4, (mem1 + mem2 + mem3 + mem4)/(1024*1024));
         
        // info!("GABEDEBUG: self.core_count is :{}",  self.core_count);
        // info!("GABEDEBUG: GPU mem1 need:{}Mbyte",  (mem1)/(1024*1024));
        // info!("GABEDEBUG: GPU mem2 need:{}Mbyte",  (mem2)/(1024*1024));
        // info!("GABEDEBUG: GPU mem3 need:{}Mbyte",  (mem3)/(1024*1024));
        // info!("GABEDEBUG: GPU mem4 need:{}Mbyte",  (mem4)/(1024*1024));




        let mut base_buffer = self.program.create_buffer::<G>(n)?;
        base_buffer.write_from(0, bases)?;
        let mut exp_buffer = self
            .program
            .create_buffer::<<<G::Engine as ScalarEngine>::Fr as PrimeField>::Repr>(n)?;
        exp_buffer.write_from(0, exps)?;
        let bucket_buffer = self
            .program
            .create_buffer::<<G as CurveAffine>::Projective>(2 * self.core_count * bucket_len)?;
        let result_buffer = self
            .program
            .create_buffer::<<G as CurveAffine>::Projective>(2 * self.core_count)?;

        // Make global work size divisible by `LOCAL_WORK_SIZE`
        let mut global_work_size = num_windows * num_groups;
        global_work_size +=
            (LOCAL_WORK_SIZE - (global_work_size % LOCAL_WORK_SIZE)) % LOCAL_WORK_SIZE;

        let kernel = self.program.create_kernel(
            if TypeId::of::<G>() == TypeId::of::<E::G1Affine>() {
                "G1_bellman_multiexp"
            } else if TypeId::of::<G>() == TypeId::of::<E::G2Affine>() {
                "G2_bellman_multiexp"
            } else {
                return Err(GPUError::Simple("Only E::G1 and E::G2 are supported!"));
            },
            global_work_size,
            None,
        );

        call_kernel!(
            kernel,
            &base_buffer,
            &bucket_buffer,
            &result_buffer,
            &exp_buffer,
            n as u32,
            num_groups as u32,
            num_windows as u32,
            window_size as u32
        )?;

        let mut results = vec![<G as CurveAffine>::Projective::zero(); num_groups * num_windows];
        result_buffer.read_into(0, &mut results)?;

        // Using the algorithm below, we can calculate the final result by accumulating the results
        // of those `NUM_GROUPS` * `NUM_WINDOWS` threads.
        let mut acc = <G as CurveAffine>::Projective::zero();
        let mut bits = 0;
        for i in 0..num_windows {
            let w = std::cmp::min(window_size, exp_bits - bits);
            for _ in 0..w {
                acc.double();
            }
            for g in 0..num_groups {
                acc.add_assign(&results[g * num_windows + i]);
            }
            bits += w; // Process the next window
        }

        Ok(acc)
    }
}

pub fn only_cpu_multiexp<G>(
    pool: &Worker,
    bases: Arc<Vec<G>>,
    exps: Arc<Vec<<<G::Engine as ScalarEngine>::Fr as PrimeField>::Repr>>,
    skip: usize,
    n: usize,
) -> GPUResult<<G as CurveAffine>::Projective>
    where
        G: CurveAffine,
        <G as groupy::CurveAffine>::Engine: crate::bls::Engine,
{
    use rayon::prelude::*;
    let mut acc = <G as CurveAffine>::Projective::zero();

    let cpu_bases = bases;
    let cpu_exps = exps;

    let n = cpu_exps.len();
    let cpu_bases = &cpu_bases[skip..(skip + n)];

    let (tx_cpu, rx_cpu) = mpsc::channel();
    let mut scoped_pool = Pool::new(1);

    let mut cpu_core_ids = vec![];
    for i in 1..64 {
        cpu_core_ids.push(
            i as usize
        )
    }

    use std::cell::Cell;
    use std::sync::Mutex;
    let mut round_counter = Arc::new(Mutex::new(1));

    scoped_pool.scoped(|scoped| {
        scoped.execute(move || {
            let counter = round_counter.clone();

            let used_core = 4;
            let per_core_chunk_size = ((cpu_bases.len() as f64) / (used_core as f64)).ceil() as usize;
            let cpu_results = if cpu_bases.len() > 0 {
                cpu_bases.par_chunks(per_core_chunk_size)
                    .zip(cpu_exps.par_chunks(per_core_chunk_size))
                    .zip(cpu_core_ids.par_chunks(per_core_chunk_size))
                    .map(|((bases, exps), core_ids)| -> Result<<G as CurveAffine>::Projective, GPUError> {
                        let mut acc = <G as CurveAffine>::Projective::zero();

                        {
                            let origin = counter.lock().unwrap();
                            info!("ZQ: start only_cpu_multiexp -> NO: {:?}", origin);
                            info!("ZQ: start only_cpu_multiexp -> bases len: {:?}, exps len: {:?}, core_ids {:?}",
                                bases.len(), exps.len(), core_ids);
                        }

                        let cpu_waiter = multiexp_with_cpu(
                            &pool,
                            (Arc::new(bases.to_vec()), 0),
                            FullDensity,
                            Arc::new(exps.to_vec()),
                            &mut None,
                            Vec::from(core_ids),
                        );

                        {
                            let mut value = counter.lock().unwrap();
                            *value += 1;
                        }

                        acc = cpu_waiter.wait().unwrap();

                        Ok(acc)
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            tx_cpu.send(cpu_results).unwrap();
        });
    });

    let cpu_results = rx_cpu.recv().unwrap();

    for r in cpu_results {
        acc.add_assign(&r?);
    }

    Ok(acc)
}

// A struct that containts several multiexp kernels for different devices
pub struct MultiexpKernel<E>
where
    E: Engine,
{
    kernels: Vec<SingleMultiexpKernel<E>>,
    _lock: locks::GPULock, // RFC 1857: struct fields are dropped in the same order as they are declared.
}

impl<E> MultiexpKernel<E>
where
    E: Engine,
{
    pub fn create(priority: bool) -> GPUResult<MultiexpKernel<E>> {
        let lock = locks::GPULock::lock();

        let devices = opencl::Device::all()?;

        let kernels: Vec<_> = devices
            .into_iter()
            .map(|d| (d.clone(), SingleMultiexpKernel::<E>::create(d, priority)))
            .filter_map(|(device, res)| {
                if let Err(ref e) = res {
                    error!(
                        "Cannot initialize kernel for device '{}'! Error: {}",
                        device.name(),
                        e
                    );
                }
                res.ok()
            })
            .collect();

        if kernels.is_empty() {
            return Err(GPUError::Simple("No working GPUs found!"));
        }
        info!(
            "Multiexp: {} working device(s) selected. (CPU utilization: {})",
            kernels.len(),
            get_cpu_utilization()
        );
        for (i, k) in kernels.iter().enumerate() {
            info!(
                "Multiexp: Device {}: {} (Chunk-size: {})",
                i,
                k.program.device().name(),
                k.n
            );
        }
        Ok(MultiexpKernel::<E> {
            kernels,
            _lock: lock,
        })
    }

    pub fn multiexp<G>(
        &mut self,
        pool: &Worker,
        bases: Arc<Vec<G>>,
        exps: Arc<Vec<<<G::Engine as ScalarEngine>::Fr as PrimeField>::Repr>>,
        skip: usize,
        n: usize,
    ) -> GPUResult<<G as CurveAffine>::Projective>
    where
        G: CurveAffine,
        <G as groupy::CurveAffine>::Engine: crate::bls::Engine,
    {
        let num_devices = self.kernels.len();
        // Bases are skipped by `self.1` elements, when converted from (Arc<Vec<G>>, usize) to Source
        // https://github.com/zkcrypto/bellman/blob/10c5010fd9c2ca69442dc9775ea271e286e776d8/src/multiexp.rs#L38
        let bases = &bases[skip..(skip + n)];
        let exps = &exps[..n];
        let cpu_n = ((n as f64) * get_cpu_utilization()) as usize;
        let n = n - cpu_n;
        let (cpu_bases, bases) = bases.split_at(cpu_n);
        let (cpu_exps, exps) = exps.split_at(cpu_n);
        let chunk_size = ((n as f64) / (num_devices as f64)).ceil() as usize;
        //ZQ: h_s的
        //这个是总的，In multiexp chunk_size is ---- :134217727， 现在拆分成20000000一次，循环7次

        //ZQ: l_s start的
        //In multiexp chunk_size is ---- :130169893，会有写差异


        // ZQ: inputs start
        // In multiexp chunk_size is ---- :129753292
        info!("In multiexp chunk_size is ---- :{}",  chunk_size);

        // let chunk_size = 20000000;

        crate::multicore::THREAD_POOL.install(|| {
            use rayon::prelude::*;
            extern crate core_affinity;

            let mut acc = <G as CurveAffine>::Projective::zero();

            // concurrent computing
            let (tx_gpu, rx_gpu) = mpsc::channel();
            //let (tx_cpu, rx_cpu) = mpsc::channel();
            let mut scoped_pool = Pool::new(2);

            let mut gpu_core_ids = vec![core_affinity::CoreId{id: 125 as usize}, core_affinity::CoreId{id: 126 as usize}];

            rayon::scope(|scoped| {
                // GPU
                scoped.spawn(move |_| {
                    let results = if n > 0 {
                        bases
                            .par_chunks(chunk_size)
                            .zip(exps.par_chunks(chunk_size))
                            .zip(self.kernels.par_iter_mut())
                            .zip(gpu_core_ids.par_iter_mut())
                            .map(|(((bases, exps), kern), core_id)| -> Result<<G as CurveAffine>::Projective, GPUError> {
                                // core_affinity::set_for_current(*core_id);
                                info!("ZQ: force set cpu coreID: {:?}", core_id);
                                info!("ZQ: bases len: {:?}, exps len: {:?}", bases.len(), exps.len());

                                let mut acc = <G as CurveAffine>::Projective::zero();
                                let jack_chunk_3080 = 33554466;
                                let mut jack_windows_size = 11;
                                let size_result = std::mem::size_of::<<G as CurveAffine>::Projective>();
                                if size_result > 144 {
                                    jack_windows_size = 8;
                                }
                                for (bases, exps) in bases.chunks(jack_chunk_3080).zip(exps.chunks(jack_chunk_3080)) {
                                    let result = kern.multiexp(bases, exps, bases.len(), jack_windows_size)?;
                                    acc.add_assign(&result);
                                }

                                Ok(acc)
                            })
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };

                    tx_gpu.send(results).unwrap();

                });

                /*
                // CPU
                scoped.execute(move || {
                    let used_core = 128;
                    let per_core_chunk_size = ((cpu_bases.len() as f64) / (used_core as f64)).ceil() as usize;
                    let cpu_results = if cpu_bases.len() > 0 {
                        cpu_bases.par_chunks(per_core_chunk_size)
                            .zip(cpu_exps.par_chunks(per_core_chunk_size))
                            .map(|(bases, exps)| -> Result<<G as CurveAffine>::Projective, GPUError> {
                                let mut acc = <G as CurveAffine>::Projective::zero();

                                let cpu_waiter = cpu_multiexp(
                                    &pool,
                                    (Arc::new(bases.to_vec()), 0),
                                    FullDensity,
                                    Arc::new(exps.to_vec()),
                                    &mut None,
                                );

                                acc = cpu_waiter.wait().unwrap();

                                Ok(acc)
                            })
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };

                    tx_cpu.send(cpu_results).unwrap();
                });

                 */

                /*
                // CPU
                scoped.execute(move || {
                    let cpu_acc = cpu_multiexp(
                        &pool,
                        (Arc::new(cpu_bases.to_vec()), 0),
                        FullDensity,
                        Arc::new(cpu_exps.to_vec()),
                        &mut None,
                    );
                    let cpu_r = cpu_acc.wait().unwrap();

                    tx_cpu.send(cpu_r).unwrap();
                });
                 */
            });

            // waiting results...
            let gpu_results = rx_gpu.recv().unwrap();
            //let cpu_results = rx_cpu.recv().unwrap();

            for r in gpu_results {
                acc.add_assign(&r?);
            }

            //for r in cpu_results {
            //    acc.add_assign(&r?);
            //}

            /*
            // waiting results...
            let results = rx_gpu.recv().unwrap();
            let cpu_r = rx_cpu.recv().unwrap();

            for r in results {
                acc.add_assign(&r?);
            }
            acc.add_assign(&cpu_r);
             */
            
            Ok(acc)
        })
    }
}