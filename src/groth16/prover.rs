use std::sync::Arc;
use std::time::Instant;
// use std::thread;
use std::sync::mpsc;

use crate::bls::Engine;
use ff::{Field, PrimeField};
use groupy::{CurveAffine, CurveProjective};
use rand_core::RngCore;
use rayon::prelude::*;

use super::{ParameterSource, Proof};
use crate::domain::{EvaluationDomain, Scalar};
use crate::gpu::{LockedFFTKernel, LockedMultiexpKernel, LockedFFTKernel_1};
use crate::multicore::{Worker, THREAD_POOL, Waiter};
use crate::multiexp::{multiexp, multiexp_fulldensity, multiexp_fulldensity_only_cpu, density_filter, multiexp_skipdensity, DensityTracker, FullDensity, SourceBuilder};
use crate::{
    Circuit, ConstraintSystem, Index, LinearCombination, SynthesisError, Variable, BELLMAN_VERSION,
};
use log::info;

// use crossbeam_channel::{bounded, Receiver};

extern crate scoped_threadpool;
use scoped_threadpool::Pool;

#[cfg(feature = "gpu")]
use crate::gpu::PriorityLock;

fn eval<E: Engine>(
    lc: &LinearCombination<E>,
    mut input_density: Option<&mut DensityTracker>,
    mut aux_density: Option<&mut DensityTracker>,
    input_assignment: &[E::Fr],
    aux_assignment: &[E::Fr],
) -> E::Fr {
    let mut acc = E::Fr::zero();

    for (&index, &coeff) in lc.0.iter() {
        let mut tmp;

        match index {
            Variable(Index::Input(i)) => {
                tmp = input_assignment[i];
                if let Some(ref mut v) = input_density {
                    v.inc(i);
                }
            }
            Variable(Index::Aux(i)) => {
                tmp = aux_assignment[i];
                if let Some(ref mut v) = aux_density {
                    v.inc(i);
                }
            }
        }

        if coeff == E::Fr::one() {
            acc.add_assign(&tmp);
        } else {
            tmp.mul_assign(&coeff);
            acc.add_assign(&tmp);
        }
    }

    acc
}

struct ProvingAssignment<E: Engine> {
    // Density of queries
    a_aux_density: DensityTracker,
    b_input_density: DensityTracker,
    b_aux_density: DensityTracker,

    // Evaluations of A, B, C polynomials
    a: Vec<Scalar<E>>,
    b: Vec<Scalar<E>>,
    c: Vec<Scalar<E>>,

    // Assignments of variables
    input_assignment: Vec<E::Fr>,
    aux_assignment: Vec<E::Fr>,
}
use std::fmt;

impl<E: Engine> fmt::Debug for ProvingAssignment<E> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("ProvingAssignment")
            .field("a_aux_density", &self.a_aux_density)
            .field("b_input_density", &self.b_input_density)
            .field("b_aux_density", &self.b_aux_density)
            .field(
                "a",
                &self
                    .a
                    .iter()
                    .map(|v| format!("Fr({:?})", v.0))
                    .collect::<Vec<_>>(),
            )
            .field(
                "b",
                &self
                    .b
                    .iter()
                    .map(|v| format!("Fr({:?})", v.0))
                    .collect::<Vec<_>>(),
            )
            .field(
                "c",
                &self
                    .c
                    .iter()
                    .map(|v| format!("Fr({:?})", v.0))
                    .collect::<Vec<_>>(),
            )
            .field("input_assignment", &self.input_assignment)
            .field("aux_assignment", &self.aux_assignment)
            .finish()
    }
}

impl<E: Engine> PartialEq for ProvingAssignment<E> {
    fn eq(&self, other: &ProvingAssignment<E>) -> bool {
        self.a_aux_density == other.a_aux_density
            && self.b_input_density == other.b_input_density
            && self.b_aux_density == other.b_aux_density
            && self.a == other.a
            && self.b == other.b
            && self.c == other.c
            && self.input_assignment == other.input_assignment
            && self.aux_assignment == other.aux_assignment
    }
}

impl<E: Engine> ConstraintSystem<E> for ProvingAssignment<E> {
    type Root = Self;

    fn new() -> Self {
        Self {
            a_aux_density: DensityTracker::new(),
            b_input_density: DensityTracker::new(),
            b_aux_density: DensityTracker::new(),
            a: vec![],
            b: vec![],
            c: vec![],
            input_assignment: vec![],
            aux_assignment: vec![],
        }
    }

    fn alloc<F, A, AR>(&mut self, _: A, f: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError>,
        A: FnOnce() -> AR,
        AR: Into<String>,
    {
        self.aux_assignment.push(f()?);
        self.a_aux_density.add_element();
        self.b_aux_density.add_element();

        Ok(Variable(Index::Aux(self.aux_assignment.len() - 1)))
    }

    fn alloc_input<F, A, AR>(&mut self, _: A, f: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError>,
        A: FnOnce() -> AR,
        AR: Into<String>,
    {
        self.input_assignment.push(f()?);
        self.b_input_density.add_element();

        Ok(Variable(Index::Input(self.input_assignment.len() - 1)))
    }

    fn enforce<A, AR, LA, LB, LC>(&mut self, _: A, a: LA, b: LB, c: LC)
    where
        A: FnOnce() -> AR,
        AR: Into<String>,
        LA: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
        LB: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
        LC: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
    {
        let a = a(LinearCombination::zero());
        let b = b(LinearCombination::zero());
        let c = c(LinearCombination::zero());

        self.a.push(Scalar(eval(
            &a,
            // Inputs have full density in the A query
            // because there are constraints of the
            // form x * 0 = 0 for each input.
            None,
            Some(&mut self.a_aux_density),
            &self.input_assignment,
            &self.aux_assignment,
        )));
        self.b.push(Scalar(eval(
            &b,
            Some(&mut self.b_input_density),
            Some(&mut self.b_aux_density),
            &self.input_assignment,
            &self.aux_assignment,
        )));
        self.c.push(Scalar(eval(
            &c,
            // There is no C polynomial query,
            // though there is an (beta)A + (alpha)B + C
            // query for all aux variables.
            // However, that query has full density.
            None,
            None,
            &self.input_assignment,
            &self.aux_assignment,
        )));
    }

    fn push_namespace<NR, N>(&mut self, _: N)
    where
        NR: Into<String>,
        N: FnOnce() -> NR,
    {
        // Do nothing; we don't care about namespaces in this context.
    }

    fn pop_namespace(&mut self) {
        // Do nothing; we don't care about namespaces in this context.
    }

    fn get_root(&mut self) -> &mut Self::Root {
        self
    }

    fn is_extensible() -> bool {
        true
    }

    fn extend(&mut self, other: Self) {
        self.a_aux_density.extend(other.a_aux_density, false);
        self.b_input_density.extend(other.b_input_density, true);
        self.b_aux_density.extend(other.b_aux_density, false);

        self.a.extend(other.a);
        self.b.extend(other.b);
        self.c.extend(other.c);

        self.input_assignment
            // Skip first input, which must have been a temporarily allocated one variable.
            .extend(&other.input_assignment[1..]);
        self.aux_assignment.extend(other.aux_assignment);
    }
}

pub fn create_random_proof_batch_priority<E, C, R, P: ParameterSource<E>>(
    circuits: Vec<C>,
    params: P,
    rng: &mut R,
    priority: bool,
) -> Result<Vec<Proof<E>>, SynthesisError>
where
    E: Engine,
    C: Circuit<E> + Send,
    R: RngCore,
{
    let r_s = (0..circuits.len()).map(|_| E::Fr::random(rng)).collect();
    let s_s = (0..circuits.len()).map(|_| E::Fr::random(rng)).collect();

    create_proof_batch_priority::<E, C, P>(circuits, params, r_s, s_s, priority)
}

pub fn create_proof_batch_priority<E, C, P: ParameterSource<E>>(
    circuits: Vec<C>,
    params: P,
    r_s: Vec<E::Fr>,
    s_s: Vec<E::Fr>,
    priority: bool,
) -> Result<Vec<Proof<E>>, SynthesisError>
where
    E: Engine,
    C: Circuit<E> + Send,
{
    info!("Bellperson {} is being used!", BELLMAN_VERSION);

    THREAD_POOL.install(|| create_proof_batch_priority_inner(circuits, params, r_s, s_s, priority))
}

/*
fn print_type_of<T>(_: &T) -> String {
    format!("{}", unsafe { std::intrinsics::type_name::<T>() })
}
 */

fn create_proof_batch_priority_inner<E, C, P: ParameterSource<E>>(
    circuits: Vec<C>,
    params: P,
    r_s: Vec<E::Fr>,
    s_s: Vec<E::Fr>,
    priority: bool,
) -> Result<Vec<Proof<E>>, SynthesisError>
where
    E: Engine,
    C: Circuit<E> + Send,
{


    let start = Instant::now();

    let now = Instant::now();
    info!("ZQ: build provers start");
    // 生成电路的约束系统，这一步会回调每个电路不同的电路逻辑
    let mut provers = circuits
        .into_par_iter()
        .map(|circuit| -> Result<_, SynthesisError> {
            let mut prover = ProvingAssignment::new();
            prover.alloc_input(|| "", || Ok(E::Fr::one()))?;
            circuit.synthesize(&mut prover)?;
            for i in 0..prover.input_assignment.len() {
                prover.enforce(|| "", |lc| lc + Variable(Index::Input(i)), |lc| lc, |lc| lc);
            }
            Ok(prover)
        })
        .collect::<Result<Vec<_>, _>>()?;
    info!("ZQ: build provers end: {:?}", now.elapsed());

    // Start prover timer
    info!("ZQ: starting proof timer");
    let worker = Worker::new();
    let input_len = provers[0].input_assignment.len();
    let vk = params.get_vk(input_len)?;
    let n = provers[0].a.len();


    // Make sure all circuits have the same input len.
    for prover in &provers {
        assert_eq!(
            prover.a.len(),
            n,
            "only equaly sized circuits are supported"
        );
    }

    let mut log_d = 0;
    while (1 << log_d) < n {
        log_d += 1;
    }

    // get params
    // 读取setup阶段提前生成的参数文件
    info!("ZQ: get params start");
    let now = Instant::now();
    let (tx_h, rx_h) = mpsc::channel();
    let (tx_l, rx_l) = mpsc::channel();
    let (tx_a, rx_a) = mpsc::channel();
    let (tx_bg1, rx_bg1) = mpsc::channel();
    let (tx_bg2, rx_bg2) = mpsc::channel();
    let (tx_assignments, rx_assignments) = mpsc::channel();
    let input_assignment_len = provers[0].input_assignment.len();
    let mut pool = Pool::new(6);
    pool.scoped(|scoped| {
        let params = &params;
        let provers = &mut provers;

        // h_params
        scoped.execute(move || {
            let h_params = params.get_h(0).unwrap();
            tx_h.send(h_params).unwrap();
        });
        // l_params
        scoped.execute(move || {
            let l_params = params.get_l(0).unwrap();
            tx_l.send(l_params).unwrap();
        });

        // a_params
        scoped.execute(move || {
            let (a_inputs_source, a_aux_source) = params.get_a(input_assignment_len,0).unwrap();
            tx_a.send((a_inputs_source, a_aux_source)).unwrap();
        });

        // bg1_params
        scoped.execute(move || {
            let (b_g1_inputs_source, b_g1_aux_source) = params.get_b_g1(1,0).unwrap();
            tx_bg1.send((b_g1_inputs_source, b_g1_aux_source)).unwrap();
        });
        // bg2_params
        scoped.execute(move || {
            let (b_g2_inputs_source, b_g2_aux_source) = params.get_b_g2(1,0).unwrap();
            tx_bg2.send((b_g2_inputs_source, b_g2_aux_source)).unwrap();
        });

        // assignments
        scoped.execute(move || {

            let assignments = provers
                .par_iter_mut()
                .map(|prover| {
                    let _input_assignment = std::mem::replace(&mut prover.input_assignment, Vec::new());
                    let _aux_assignment = std::mem::replace(&mut prover.aux_assignment, Vec::new());
                    let input_assignment = Arc::new(
                        _input_assignment
                            .into_iter()
                            .map(|s| s.into_repr())
                            .collect::<Vec<_>>(),
                    );
                    let aux_assignment = Arc::new(
                        _aux_assignment
                            .into_iter()
                            .map(|s| s.into_repr())
                            .collect::<Vec<_>>(),
                    );
                    (input_assignment, aux_assignment)
                })
                .collect::<Vec<_>>();

            tx_assignments.send(assignments).unwrap();
        });
    });

    // waiting params
    info!("ZQ: waiting params...");
    let h_params = rx_h.recv().unwrap();
    let l_params = rx_l.recv().unwrap();
    let (a_inputs_source, a_aux_source) = rx_a.recv().unwrap();
    let (b_g1_inputs_source, b_g1_aux_source) = rx_bg1.recv().unwrap();
    let (b_g2_inputs_source, b_g2_aux_source) = rx_bg2.recv().unwrap();
    let assignments = rx_assignments.recv().unwrap();
    info!("ZQ: get params end: {:?}", now.elapsed());

    #[cfg(feature = "gpu")]
    let prio_lock = if priority {
        Some(PriorityLock::lock())
    } else {
        None
    };


    info!("ZQ: a_s start");
    info!("ZQ: a_s provers length: {:?}", provers.len());
    // iFFT生成三个多项式系数，cosetFFT生成多项式在coset处的取值.
    let now = Instant::now();
    let mut fft_kern = Some(LockedFFTKernel::<E>::new(log_d, priority));
    let mut fft_kern_1 = Some(LockedFFTKernel_1::<E>::new(log_d, priority));
    let mut pool = Pool::new(3);
    let a_s = provers
        .iter_mut()
        .map(|prover| {
            let mut a =
                EvaluationDomain::from_coeffs(std::mem::replace(&mut prover.a, Vec::new()))?;
            let mut b =
                EvaluationDomain::from_coeffs(std::mem::replace(&mut prover.b, Vec::new()))?;
            let mut c =
                EvaluationDomain::from_coeffs(std::mem::replace(&mut prover.c, Vec::new()))?;

            let now = Instant::now();
            pool.scoped(|scoped| {

                scoped.execute( || {
                    a.ifft(&worker, &mut fft_kern).unwrap();
                    a.coset_fft(&worker, &mut fft_kern).unwrap();
                });

                scoped.execute( || {
                    b.ifft_1(&worker, &mut fft_kern_1).unwrap();
                    b.coset_fft_1(&worker, &mut fft_kern_1).unwrap();
                });

            });

            c.ifft(&worker, &mut fft_kern).unwrap();
            c.coset_fft(&worker, &mut fft_kern).unwrap();

            info!("ZQ: a_s phase 1 duration: {:?}", now.elapsed());


            // a * b - c / z
            a.mul_assign(&worker, &b);
            drop(b);
            a.sub_assign(&worker, &c);
            drop(c);
            a.divide_by_z_on_coset(&worker);
            a.icoset_fft(&worker, &mut fft_kern)?;

            let mut a = a.into_coeffs();
            let a_len = a.len() - 1;
            a.truncate(a_len);

            Ok(Arc::new(a.into_par_iter().map(|s| s.0.into_repr()).collect::<Vec<_>>()))
        })
        .collect::<Result<Vec<_>, SynthesisError>>()?;
    info!("ZQ: a_s end: {:?}", now.elapsed());
    drop(fft_kern);
    drop(fft_kern_1);

    /*
    info!("ZQ: h_s start");
    let now = Instant::now();
    let h_s = a_s
        .into_iter()
        .map(|a| {
            let h = multiexp_fulldensity(
                &worker,
                h_params.clone(),
                FullDensity,
                a,
                &mut multiexp_kern,
            );
            Ok(h)
        })
        .collect::<Result<Vec<_>, SynthesisError>>()?;
    info!("ZQ: h_s end: {:?}", now.elapsed());
     */


    /*******************************************************************************/

    let h_s_start = Instant::now();
    info!("ZQ h_s start");
    // 把之前计算的数（多项式值），映射到椭圆曲线上。

    let percent = 2;
    let cpu_a_s = &a_s[0..percent];
    let gpu_a_s = &a_s[percent..];

    use scoped_threadpool::Pool;
    let mut cpu_gpu_pool = Pool::new(2);

    let (h_s_tx_cpu, h_s_rx_cpu) = mpsc::channel();
    let (h_s_tx_gpu, h_s_rx_gpu) = mpsc::channel();

    cpu_gpu_pool.scoped(|scoped| {
        let worker_cpu = worker.clone();
        let params_cpu = h_params.clone();

        // cpu work list
        scoped.execute(move || {
            let h_s_cpu_start = Instant::now();
            info!("ZQ h_s cpu start");
            
            let first = cpu_a_s.get(0).unwrap().clone();
            let result = multiexp_fulldensity_only_cpu(
                &worker_cpu,
                params_cpu.clone(),
                FullDensity,
                first);
            h_s_tx_cpu.send(result).unwrap();

            info!("ZQ h_s cpu round 1 end");

            let first = cpu_a_s.get(1).unwrap().clone();
            let result = multiexp_fulldensity_only_cpu(
                &worker_cpu,
                params_cpu.clone(),
                FullDensity,
                first);
            h_s_tx_cpu.send(result).unwrap();

            info!("ZQ h_s cpu round 2 end");
            info!("ZQ h_s cpu end: {:?}", h_s_cpu_start.elapsed());
        });

        let worker_gpu = worker.clone();
        let mut params_gpu = h_params.clone();
        // gpu work list
        scoped.execute(move || {
            let h_s_gpu_start = Instant::now();
            info!("ZQ h_s gpu start");
            let mut i = 1;

            let mut multiexp_kern = Some(LockedMultiexpKernel::<E>::new(log_d, priority));

            let mut gpu_result_list = gpu_a_s
                .into_iter()
                .map(|a| {
                    info!("ZQ h_s gpu round: {:?}", i);
                    let h = multiexp_fulldensity(
                        &worker_gpu,
                        params_gpu.clone(),
                        FullDensity,
                        a.clone(),
                        &mut multiexp_kern,
                    );
                    i += 1;
                    Ok(h)
                })
                .collect::<Result<Vec<_>, SynthesisError>>();

            if let Ok(result_list) = gpu_result_list {
                for item in result_list {
                    h_s_tx_gpu.send(item.wait()).unwrap();
                }
            }

            info!("ZQ h_s gpu end: {:?}", h_s_gpu_start.elapsed());
        });
    });

    let mut h_s = Vec::new();
    for result in h_s_rx_cpu.recv() {
        h_s.push(Waiter::done(result));
    }

    for result in h_s_rx_gpu.recv() {
        h_s.push(Waiter::done(result));
    }

    info!("ZQ h_s end: {:?}", h_s_start.elapsed());

    let mut multiexp_kern = Some(LockedMultiexpKernel::<E>::new(log_d, priority));

    /*
    let first = first_as.get(0).unwrap().clone();
    let result = multiexp_fulldensity_only_cpu(
                &worker,
                h_params.clone(),
                FullDensity,
                first);

    info!("ZQ: h_s start");
    let now = Instant::now();
    let mut h_s = other_as
        .into_iter()
        .map(|a| {
            let h = multiexp_fulldensity(
                &worker,
                h_params.clone(),
                FullDensity,
                a.clone(),
                &mut multiexp_kern,
            );
            Ok(h)
        })
        .collect::<Result<Vec<_>, SynthesisError>>()?;
    info!("ZQ: h_s end: {:?}", now.elapsed());

    h_s.insert(0, result);

     */

    /*******************************************************************************/

    let l_s_start = Instant::now();
    info!("ZQ l_s start");

    let percent = 2;
    let cpu_l_s = &assignments[0..percent];
    let gpu_l_s = &assignments[percent..];
    let cpu_l_s = cpu_l_s.to_vec();

    let mut cpu_gpu_pool = Pool::new(2);

    let (l_s_tx_cpu, l_s_rx_cpu) = mpsc::channel();
    let (l_s_tx_gpu, l_s_rx_gpu) = mpsc::channel();

    let worker_cpu = worker.clone();
    let params_cpu = l_params.clone();
    cpu_gpu_pool.scoped(|scoped| {
        let worker_cpu = worker.clone();
        let params_cpu = l_params.clone();

        info!("ZQ l_s cpu start");
        scoped.execute(move || {
            let (_, first) = cpu_l_s.get(0).unwrap().clone();
            let result = multiexp_fulldensity_only_cpu(
                &worker_cpu,
                params_cpu.clone(),
                FullDensity,
                first);

            let (_, second) = cpu_l_s.get(1).unwrap().clone();
            let result = multiexp_fulldensity_only_cpu(
                &worker_cpu,
                params_cpu.clone(),
                FullDensity,
                second);

            l_s_tx_cpu.send(result).unwrap();
        });

        let worker_gpu = worker.clone();
        let mut params_gpu = l_params.clone();
        scoped.execute(move || {
            let h_s_gpu_start = Instant::now();
            info!("ZQ l_s gpu start");
            let mut i = 1;

            let mut multiexp_kern = Some(LockedMultiexpKernel::<E>::new(log_d, priority));

            let mut gpu_result_list = gpu_l_s
                .into_iter()
                .map(|(_, aux_assignment)| {
                    info!("ZQ l_s gpu round: {:?}", i);
                    let h = multiexp_fulldensity(
                        &worker_gpu,
                        params_gpu.clone(),
                        FullDensity,
                        aux_assignment.clone(),
                        &mut multiexp_kern,
                    );
                    i += 1;
                    Ok(h)
                })
                .collect::<Result<Vec<_>, SynthesisError>>();

            if let Ok(result_list) = gpu_result_list {
                for item in result_list {
                    l_s_tx_gpu.send(item.wait()).unwrap();
                }
            }

            info!("ZQ l_s gpu end: {:?}", h_s_gpu_start.elapsed());
        });
    });

    let mut l_s = Vec::new();
    for result in l_s_rx_cpu.recv() {
        l_s.push(Waiter::done(result));
    }

    for result in l_s_rx_gpu.recv() {
        l_s.push(Waiter::done(result));
    }

    info!("ZQ l_s end: {:?}", h_s_start.elapsed());

    /*
    info!("ZQ: l_s start");
    info!("ZQ: l_s assignments length: {:?}", assignments.len());
    // 把之前计算的数（多项式值），映射到椭圆曲线上。
    let now = Instant::now();
    let l_s = assignments
        .iter()
        .map(|(_,aux_assignment)| {
            let l = multiexp_fulldensity(
                &worker,
                l_params.clone(),
                FullDensity,
                aux_assignment.clone(),
                &mut multiexp_kern,
            );
            Ok(l)
        })
        .collect::<Result<Vec<_>, SynthesisError>>()?;
    info!("ZQ: l_s end: {:?}", now.elapsed());
     */


    info!("ZQ: inputs start");
    info!("ZQ: inputs length: {:?}", provers.len());
    // 处理电路的公开和私有输入
    let now = Instant::now();
    let inputs = provers
        .into_iter()
        .zip(assignments.into_iter())
        .map(|(prover, (input_assignment,aux_assignment))| {
            let b_input_density = Arc::new(prover.b_input_density);
            let b_aux_density = Arc::new(prover.b_aux_density);

            let start = Instant::now();
            let a_inputs = multiexp_fulldensity(
                &worker,
                a_inputs_source.clone(),
                FullDensity,
                input_assignment.clone(),
                &mut multiexp_kern,
            );
            info!("ZQ: inputs phase 1: {:?}", start.elapsed());

            let start = Instant::now();
            let (
                a_aux_bss,
                a_aux_exps,
                a_aux_skip,
                a_aux_n
            ) = density_filter(
                a_aux_source.clone(),
                Arc::new(prover.a_aux_density),
                aux_assignment.clone()
            );
            let a_aux = multiexp_skipdensity(
                &worker,
                a_aux_bss,
                a_aux_exps,
                a_aux_skip,
                a_aux_n,
                &mut multiexp_kern,
            );
            info!("ZQ: inputs phase 2: {:?}", start.elapsed());

            let start = Instant::now();
            let b_g1_inputs = multiexp(
                &worker,
                b_g1_inputs_source.clone(),
                b_input_density.clone(),
                input_assignment.clone(),
                &mut multiexp_kern,
            );
            info!("ZQ: inputs phase 3: {:?}", start.elapsed());

            let start = Instant::now();
            let (
                b_g1_aux_bss,
                b_g1_aux_exps,
                b_g1_aux_skip,
                b_g1_aux_n
            ) = density_filter(
                b_g1_aux_source.clone(),
                b_aux_density.clone(),
                aux_assignment.clone()
            );
            let b_g1_aux = multiexp_skipdensity(
                &worker,
                b_g1_aux_bss,
                b_g1_aux_exps,
                b_g1_aux_skip,
                b_g1_aux_n,
                &mut multiexp_kern,
            );
            info!("ZQ: inputs phase 4: {:?}", start.elapsed());

            let start = Instant::now();
            let b_g2_inputs = multiexp(
                &worker,
                b_g2_inputs_source.clone(),
                b_input_density.clone(),
                input_assignment.clone(),
                &mut multiexp_kern,
            );
            info!("ZQ: inputs phase 5: {:?}", start.elapsed());

            let start = Instant::now();
            let (
                b_g2_aux_bss,
                b_g2_aux_exps,
                b_g2_aux_skip,
                b_g2_aux_n
            ) = density_filter(
                b_g2_aux_source.clone(),
                b_aux_density.clone(),
                aux_assignment.clone()
            );
            let b_g2_aux = multiexp_skipdensity(
                &worker,
                b_g2_aux_bss,
                b_g2_aux_exps,
                b_g2_aux_skip,
                b_g2_aux_n,
                &mut multiexp_kern,
            );
            info!("ZQ: inputs phase 6: {:?}", start.elapsed());

            Ok((
                a_inputs,
                a_aux,
                b_g1_inputs,
                b_g1_aux,
                b_g2_inputs,
                b_g2_aux,
            ))
        })
        .collect::<Result<Vec<_>, SynthesisError>>()?;
    info!("ZQ: inputs end: {:?}", now.elapsed());

    drop(multiexp_kern);
    #[cfg(feature = "gpu")]
    drop(prio_lock);


    info!("ZQ: proofs start");
    // 生成证明
    let now = Instant::now();
    let proofs = h_s
        .into_iter()
        .zip(l_s.into_iter())
        .zip(inputs.into_iter())
        .zip(r_s.into_iter())
        .zip(s_s.into_iter())
        .map(
            |(
                (((h, l), (a_inputs, a_aux, b_g1_inputs, b_g1_aux, b_g2_inputs, b_g2_aux)), r),
                s,
            )| {
                if vk.delta_g1.is_zero() || vk.delta_g2.is_zero() {
                    // If this element is zero, someone is trying to perform a
                    // subversion-CRS attack.
                    return Err(SynthesisError::UnexpectedIdentity);
                }

                let mut g_a = vk.delta_g1.mul(r);
                g_a.add_assign_mixed(&vk.alpha_g1);
                let mut g_b = vk.delta_g2.mul(s);
                g_b.add_assign_mixed(&vk.beta_g2);
                let mut g_c;
                {
                    let mut rs = r;
                    rs.mul_assign(&s);

                    g_c = vk.delta_g1.mul(rs);
                    g_c.add_assign(&vk.alpha_g1.mul(s));
                    g_c.add_assign(&vk.beta_g1.mul(r));
                }
                let mut a_answer = a_inputs.wait()?;
                a_answer.add_assign(&a_aux.wait()?);
                g_a.add_assign(&a_answer);
                a_answer.mul_assign(s);
                g_c.add_assign(&a_answer);

                let mut b1_answer = b_g1_inputs.wait()?;
                b1_answer.add_assign(&b_g1_aux.wait()?);
                let mut b2_answer = b_g2_inputs.wait()?;
                b2_answer.add_assign(&b_g2_aux.wait()?);

                g_b.add_assign(&b2_answer);
                b1_answer.mul_assign(r);
                g_c.add_assign(&b1_answer);
                g_c.add_assign(&h.wait()?);
                g_c.add_assign(&l.wait()?);

                Ok(Proof {
                    a: g_a.into_affine(),
                    b: g_b.into_affine(),
                    c: g_c.into_affine(),
                })
            },
        )
        .collect::<Result<Vec<_>, SynthesisError>>()?;
    info!("ZQ: proofs end: {:?}", now.elapsed());

    info!("ZQ: prover time: {:?}", start.elapsed());

    Ok(proofs)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bls::{Bls12, Fr};
    use rand::Rng;
    use rand_core::SeedableRng;
    use rand_xorshift::XorShiftRng;

    #[test]
    fn test_proving_assignment_extend() {
        let mut rng = XorShiftRng::from_seed([
            0x59, 0x62, 0xbe, 0x5d, 0x76, 0x3d, 0x31, 0x8d, 0x17, 0xdb, 0x37, 0x32, 0x54, 0x06,
            0xbc, 0xe5,
        ]);

        for k in &[2, 4, 8] {
            for j in &[10, 20, 50] {
                let count: usize = k * j;

                let mut full_assignment = ProvingAssignment::<Bls12>::new();
                full_assignment
                    .alloc_input(|| "one", || Ok(Fr::one()))
                    .unwrap();

                let mut partial_assignments = Vec::with_capacity(count / k);
                for i in 0..count {
                    if i % k == 0 {
                        let mut p = ProvingAssignment::new();
                        p.alloc_input(|| "one", || Ok(Fr::one())).unwrap();
                        partial_assignments.push(p)
                    }

                    let index: usize = i / k;
                    let partial_assignment = &mut partial_assignments[index];

                    if rng.gen() {
                        let el = Fr::random(&mut rng);
                        full_assignment
                            .alloc(|| format!("alloc:{},{}", i, k), || Ok(el.clone()))
                            .unwrap();
                        partial_assignment
                            .alloc(|| format!("alloc:{},{}", i, k), || Ok(el))
                            .unwrap();
                    }

                    if rng.gen() {
                        let el = Fr::random(&mut rng);
                        full_assignment
                            .alloc_input(|| format!("alloc_input:{},{}", i, k), || Ok(el.clone()))
                            .unwrap();
                        partial_assignment
                            .alloc_input(|| format!("alloc_input:{},{}", i, k), || Ok(el))
                            .unwrap();
                    }

                    // TODO: LinearCombination
                }

                let mut combined = ProvingAssignment::new();
                combined.alloc_input(|| "one", || Ok(Fr::one())).unwrap();

                for assignment in partial_assignments.into_iter() {
                    combined.extend(assignment);
                }
                assert_eq!(combined, full_assignment);
            }
        }
    }
}