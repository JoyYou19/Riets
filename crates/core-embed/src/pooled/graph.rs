//! The nodes added to the stock model: masked sum pooling over the transformer's per-token
//! output, then L2 normalization, producing `[batch, dims]` sentence vectors.

use ort::editor::{Graph, Model, Node, ONNX_DOMAIN, Opset};
use ort::memory::Allocator;
use ort::operator::Attribute;
use ort::value::{Outlet, Shape, SymbolicDimensions, Tensor, TensorElementType, ValueType};

use super::{ATTENTION_MASK, OUTPUT};

/// The per-token output of Hugging Face transformer exports.
const HIDDEN_STATE: &str = "last_hidden_state";
/// Prefix for every node and initializer the pooling adds, so none collides with the model's.
const POOLING_PREFIX: &str = "corelamo_pooling/";
/// ONNX `TensorProto.DataType` of float32, the target of the mask `Cast`.
const ONNX_FLOAT: i64 = 1;
/// Oldest `ai.onnx` opset in which every operator the pooling uses has its current form.
const MIN_OPSET: u32 = 9;

/// The transformer's per-token output: `last_hidden_state`, or else the only float output
/// shaped `[batch, sequence, dims]`.
pub(super) fn hidden_state_output(outputs: &[Outlet], dims: usize) -> ort::Result<String> {
    let per_token = |output: &&Outlet| {
        matches!(
            output.dtype(),
            ValueType::Tensor { ty: TensorElementType::Float32, shape, .. }
                if shape.len() == 3 && shape[2] == super::onnx_dim(dims)
        )
    };
    if let Some(output) = outputs
        .iter()
        .filter(per_token)
        .find(|o| o.name() == HIDDEN_STATE)
    {
        return Ok(output.name().to_owned());
    }
    let mut candidates = outputs.iter().filter(per_token);
    match (candidates.next(), candidates.next()) {
        (Some(output), None) => Ok(output.name().to_owned()),
        _ => Err(ort::Error::new(format!(
            "expected one float output shaped [batch, sequence, {dims}], found: {}",
            outputs
                .iter()
                .map(Outlet::name)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Nodes that turn `hidden_state` `[batch, sequence, dims]` into L2-normalized
/// `[batch, dims]` sentence vectors, keeping the model's own inputs.
///
/// Built from operators whose form has not changed since opset 9 (`Cast`, `Reshape`,
/// `MatMul`, `Transpose`, `Sqrt`, `Max`, `Div`), because added nodes must use the opset the
/// model was exported with. `ReduceSum` and `Unsqueeze` changed signature in opset 13.
pub(super) fn pooling_model(
    inputs: Vec<(String, ValueType)>,
    hidden_state: &str,
    opset: u32,
    dims: usize,
) -> ort::Result<Model> {
    if opset < MIN_OPSET {
        return Err(ort::Error::new(format!(
            "the model uses ai.onnx opset {opset}; pooling needs {MIN_OPSET} or newer"
        )));
    }
    let name = |part: &str| format!("{POOLING_PREFIX}{part}");

    let mut graph = Graph::new()?;
    graph.set_inputs(
        inputs
            .into_iter()
            .map(|(name, dtype)| Outlet::new(name, dtype)),
    )?;
    graph.set_outputs([Outlet::new(
        OUTPUT,
        ValueType::Tensor {
            ty: TensorElementType::Float32,
            shape: Shape::new([-1, super::onnx_dim(dims)]),
            dimension_symbols: SymbolicDimensions::new(["batch_size".to_owned(), String::new()]),
        },
    )])?;

    // attention mask [b, s] int64 -> [b, 1, s] float
    graph.add_node(Node::new(
        "Cast",
        ONNX_DOMAIN,
        name("mask_float"),
        [ATTENTION_MASK.to_owned()],
        [name("mask_float")],
        [Attribute::new("to", ONNX_FLOAT)?],
    )?)?;
    graph.add_node(Node::new(
        "Reshape",
        ONNX_DOMAIN,
        name("mask_row"),
        [name("mask_float"), name("shape_b_1_s")],
        [name("mask_row")],
        [],
    )?)?;
    // Masked sum over the tokens: [b, 1, s] x [b, s, d] -> [b, 1, d]. Padding has mask 0.
    // Dividing by the token count (mean pooling) is left out: the L2 normalization below
    // removes any per-row scale, so the result is the normalized mean.
    graph.add_node(Node::new(
        "MatMul",
        ONNX_DOMAIN,
        name("sum"),
        [name("mask_row"), hidden_state.to_owned()],
        [name("sum")],
        [],
    )?)?;
    // Squared L2 norm: [b, 1, d] x [b, d, 1] -> [b, 1, 1]
    graph.add_node(Node::new(
        "Transpose",
        ONNX_DOMAIN,
        name("sum_column"),
        [name("sum")],
        [name("sum_column")],
        [Attribute::new("perm", vec![0_i64, 2, 1])?],
    )?)?;
    graph.add_node(Node::new(
        "MatMul",
        ONNX_DOMAIN,
        name("squared_norm"),
        [name("sum"), name("sum_column")],
        [name("squared_norm")],
        [],
    )?)?;
    graph.add_node(Node::new(
        "Sqrt",
        ONNX_DOMAIN,
        name("norm"),
        [name("squared_norm")],
        [name("norm")],
        [],
    )?)?;
    // An all-padding row would have norm 0; the floor keeps it finite.
    graph.add_node(Node::new(
        "Max",
        ONNX_DOMAIN,
        name("safe_norm"),
        [name("norm"), name("epsilon")],
        [name("safe_norm")],
        [],
    )?)?;
    graph.add_node(Node::new(
        "Div",
        ONNX_DOMAIN,
        name("normalized"),
        [name("sum"), name("safe_norm")],
        [name("normalized")],
        [],
    )?)?;
    // [b, 1, d] -> [b, d]
    graph.add_node(Node::new(
        "Reshape",
        ONNX_DOMAIN,
        name("output"),
        [name("normalized"), name("shape_b_d")],
        [OUTPUT.to_owned()],
        [],
    )?)?;

    // Reshape: 0 keeps that dimension, -1 takes the rest.
    graph.add_initializer(name("shape_b_1_s"), i64_initializer(&[0, 1, -1])?, false)?;
    graph.add_initializer(name("shape_b_d"), i64_initializer(&[0, -1])?, false)?;
    graph.add_initializer(name("epsilon"), f32_initializer(&[1e-12])?, false)?;

    let mut model = Model::new([Opset::new(ONNX_DOMAIN, opset)?])?;
    model.add_graph(graph)?;
    Ok(model)
}

/// Initializers must be allocated by ONNX Runtime, which takes ownership of them.
fn i64_initializer(values: &[i64]) -> ort::Result<Tensor<i64>> {
    let mut tensor = Tensor::<i64>::new(&Allocator::default(), [super::onnx_dim(values.len())])?;
    tensor.extract_tensor_mut().1.copy_from_slice(values);
    Ok(tensor)
}

fn f32_initializer(values: &[f32]) -> ort::Result<Tensor<f32>> {
    let mut tensor = Tensor::<f32>::new(&Allocator::default(), [super::onnx_dim(values.len())])?;
    tensor.extract_tensor_mut().1.copy_from_slice(values);
    Ok(tensor)
}
