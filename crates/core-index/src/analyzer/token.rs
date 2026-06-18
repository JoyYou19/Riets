use crate::types::Position;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawToken {
    pub text: String,
    pub position: Position,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub position: Position,
    pub start_byte: usize,
    pub end_byte: usize,
}
