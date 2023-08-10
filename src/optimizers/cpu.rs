use std::any::Any;

use itertools::Itertools;
use petgraph::{stable_graph::NodeIndex, visit::EdgeRef, Direction};

use crate::{
    op::{Exp2, Log2, Operator, Recip, Sin, Sqrt},
    prelude::*,
};

// Ops and optimizers specific to CPU execution

pub type CPUOptimizer = (MatMulOptimizer, UnaryFusionOptimizer);

#[derive(Debug, Default)]
pub struct MatMulOptimizer;

impl GraphOptimizer for MatMulOptimizer {
    fn optimize(&self, graph: &mut Graph) {
        // Look for the matmul pattern
        for node in graph.graph.node_indices().collect_vec() {
            // Permute
            let Some((permute, permute_shape)) = graph.graph.node_weight(node) else {
                continue;
            };
            if permute.name() != "Permute" || permute_shape.len() != 2 {
                continue;
            }
            // Expand 1
            let mut dests = graph.get_dests(node);
            if dests.len() != 1 || dests[0].1 .0.name() != "Expand" || dests[0].1 .1.len() != 3 {
                continue;
            }
            let (expand_1, _) = dests.pop().unwrap();

            // Mul
            let mut dests = graph.get_dests(expand_1);
            if dests.len() != 1 || dests[0].1 .0.name() != "Mul" || dests[0].1 .1.len() != 3 {
                continue;
            }
            let (mul, _) = dests.pop().unwrap();

            // Expand 2
            let mut srcs = graph
                .get_sources(mul)
                .into_iter()
                .filter(|(i, _)| *i != expand_1)
                .collect_vec();
            if srcs.len() != 1 || srcs[0].1 .0.name() != "Expand" || srcs[0].1 .1.len() != 3 {
                continue;
            }
            let (expand_2, (_, _)) = srcs.pop().unwrap();

            let mut dests = graph.get_dests(mul);
            if dests.len() != 1 || dests[0].1 .0.name() != "SumReduce" || dests[0].1 .1.len() != 2 {
                continue;
            }
            let (sum_reduce, _) = dests.pop().unwrap();

            if graph.no_delete.contains(&node)
                || graph.no_delete.contains(&expand_1)
                || graph.no_delete.contains(&expand_2)
                || graph.no_delete.contains(&mul)
            {
                // One of these nodes is marked to not delete, we can't remove them
                continue;
            }

            let (input_0, (_, input_0_shape)) = graph.get_sources(expand_2).pop().unwrap();
            let (input_1, (_, input_1_shape)) = graph.get_sources(node).pop().unwrap();

            // Now we have a verified matmul, let's replace it with the MatMul2D op
            let new_op = graph
                .add_op(MatMul2D, vec![input_0_shape[0], input_1_shape[1]])
                .input(input_0)
                .input(input_1)
                .finish();

            // Create edges to dests
            for (weight, dest) in graph
                .graph
                .edges_directed(sum_reduce, petgraph::Direction::Outgoing)
                .map(|e| (*e.weight(), e.target()))
                .collect_vec()
            {
                graph.graph.add_edge(new_op, dest, weight);
            }
            Graph::move_references(
                &mut graph.id_remap,
                &mut graph.no_delete,
                &mut graph.to_retrieve,
                sum_reduce,
                new_op,
            );

            // Remove the old ops
            graph.graph.remove_node(expand_1);
            graph.graph.remove_node(expand_2);
            graph.graph.remove_node(node);
            graph.graph.remove_node(mul);
            graph.graph.remove_node(sum_reduce);
        }
    }
}

#[derive(Debug)]
pub struct MatMul2D;

impl Operator for MatMul2D {
    fn name(&self) -> &'static str {
        "MatMul2D"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn process(
        &self,
        inp: Vec<(&Tensor, TensorView)>,
        i: NodeIndex,
    ) -> (Option<Tensor>, TensorView) {
        let a_shape = inp[0].1.shape.shape();
        let b_shape = inp[1].1.shape.shape();
        let a_strides = &inp[0].1.shape.views.last().unwrap().strides;
        let b_strides = &inp[1].1.shape.views.last().unwrap().strides;
        let a_data = inp[0].0.data.as_any().downcast_ref::<Vec<f32>>().unwrap();
        let b_data = inp[1].0.data.as_any().downcast_ref::<Vec<f32>>().unwrap();
        let mut c = vec![0.; a_shape[0] * b_shape[1]];
        unsafe {
            matrixmultiply::sgemm(
                a_shape[0],
                a_shape[1],
                b_shape[1],
                1.0,
                &a_data[0],
                a_strides[0] as isize,
                a_strides[1] as isize,
                &b_data[0],
                b_strides[0] as isize,
                b_strides[1] as isize,
                0.0,
                &mut c[0],
                b_shape[1] as isize,
                1,
            );
        }

        (
            Some(Tensor { data: Box::new(c) }),
            TensorView {
                tensor_id: i,
                shape: ShapeTracker::new(vec![a_shape[0], b_shape[1]]),
            },
        )
    }
}

#[derive(Debug, Default)]
pub struct UnaryFusionOptimizer;

impl GraphOptimizer for UnaryFusionOptimizer {
    fn optimize(&self, graph: &mut Graph) {
        fn is_unary(op: &dyn Any) -> Option<fn(f32) -> f32> {
            if op.is::<Exp2>() {
                Some(|i| i.exp2())
            } else if op.is::<Log2>() {
                Some(|i| i.log2())
            } else if op.is::<Recip>() {
                Some(|i| i.recip())
            } else if op.is::<Sqrt>() {
                Some(|i| i.sqrt())
            } else if op.is::<Sin>() {
                Some(|i| i.sin())
            } else {
                None
            }
        }

        // Scan through unary sequential eliminations
        for id in graph.graph.node_indices().collect_vec() {
            if graph.no_delete.contains(&id) {
                continue;
            }
            let outgoing = graph
                .graph
                .edges_directed(id, petgraph::Direction::Outgoing)
                .map(|i| i.target())
                .collect_vec();
            if outgoing.len() != 1 {
                continue;
            }
            for outgoing_target in outgoing {
                let op = graph.get_op(id).unwrap();
                let other = graph.get_op(outgoing_target).unwrap();
                let mut replaced = false;
                if let Some(f) = is_unary(op.as_any()) {
                    if let Some(of) = is_unary(other.as_any()) {
                        // Unary -> Unary
                        graph.graph.node_weight_mut(id).unwrap().0 =
                            Box::new(FusedUnary(vec![f, of]));
                        replaced = true;
                    } else if let Some(mut fused) =
                        other.as_any().downcast_ref::<FusedUnary>().cloned()
                    {
                        // Unary -> Fused
                        fused.0.insert(0, f);
                        graph.graph.node_weight_mut(id).unwrap().0 = Box::new(fused);
                        replaced = true;
                    }
                } else if let Some(mut fused) = op.as_any().downcast_ref::<FusedUnary>().cloned() {
                    if let Some(of) = is_unary(other.as_any()) {
                        // Fused -> Unary
                        fused.0.push(of);
                        graph.graph.node_weight_mut(id).unwrap().0 = Box::new(fused);
                        replaced = true;
                    } else if let Some(mut other_fused) =
                        other.as_any().downcast_ref::<FusedUnary>().cloned()
                    {
                        // Fused -> Fused
                        fused.0.append(&mut other_fused.0);
                        graph.graph.node_weight_mut(id).unwrap().0 = Box::new(fused);
                        replaced = true;
                    }
                }
                if replaced {
                    // Remove other node
                    for (edge_weight, outgoing_edge_target) in graph
                        .graph
                        .edges_directed(outgoing_target, Direction::Outgoing)
                        .map(|e| (*e.weight(), e.target()))
                        .collect_vec()
                    {
                        graph.graph.add_edge(id, outgoing_edge_target, edge_weight);
                    }

                    Graph::move_references(
                        &mut graph.id_remap,
                        &mut graph.no_delete,
                        &mut graph.to_retrieve,
                        outgoing_target,
                        id,
                    );
                    graph.graph.remove_node(outgoing_target);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct FusedUnary(Vec<fn(f32) -> f32>);

impl Operator for FusedUnary {
    fn name(&self) -> &'static str {
        "FusedUnary"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn process(
        &self,
        inp: Vec<(&Tensor, TensorView)>,
        i: NodeIndex,
    ) -> (Option<Tensor>, TensorView) {
        let (mut t, mut view) = (inp[0].0.clone(), inp[0].1.clone());
        for a in t
            .data
            .as_any_mut()
            .downcast_mut::<Vec<f32>>()
            .unwrap()
            .iter_mut()
        {
            for f in &self.0 {
                *a = (f)(*a);
            }
        }

        view.tensor_id = i;
        (Some(t), view)
    }
}

#[cfg(test)]
mod tests {
    use crate::{prelude::*, tests::assert_close_data};
    #[test]
    fn test_cpu_matmul_2_d() {
        let mut cx = Graph::new();
        let a = cx.new_tensor::<R2<2, 3>>("Input");
        a.set(vec![1., 2., 3., 1., 2., 3.]);
        let b = cx.new_tensor::<R2<3, 3>>("Input");
        b.set(vec![1., 2., 3., 1., 2., 3., 1., 2., 3.]);
        let c = a.matmul(b);
        c.mark();

        cx.execute();

        let (unoptimized_c, unoptimized_c_view) =
            (c.retrieve().unwrap(), c.view().unwrap().clone());

        cx.optimize(<(CPUOptimizer, GenericOptimizer)>::default());
        cx.execute();

        assert_close_data(
            &c.retrieve().unwrap().real_data(c.view().unwrap()).unwrap(),
            &unoptimized_c.real_data(&unoptimized_c_view).unwrap(),
        );
    }
}
