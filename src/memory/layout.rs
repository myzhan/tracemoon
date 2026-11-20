//! Standard 64-bit Lua 5.4 heap ABI. Verified by tests/c/lua54_heap_layout.c.
//! Deliberately separate from CPU stack layouts; 5.5 heap decoding is not enabled yet.

pub const GLOBAL: usize = 24;
pub const REGISTRY: usize = 64;
pub const LISTS: [(usize, &str); 4] = [
    (112, "allgc"),
    (128, "finobj"),
    (176, "tobefnz"),
    (184, "fixedgc"),
];
pub const MAIN_THREAD: usize = 264;
pub const MEMERR: usize = 272;
pub const TM_NAMES: usize = 280;
pub const METATABLES: usize = 480;

pub const TABLE_SIZE: u64 = 56;
pub const TABLE_FLAGS: usize = 10;
pub const TABLE_LOG_NODES: usize = 11;
pub const TABLE_LIMIT: usize = 12;
pub const TABLE_ARRAY: usize = 16;
pub const TABLE_NODE: usize = 24;
pub const TABLE_LASTFREE: usize = 32;
pub const TABLE_META: usize = 40;
pub const NODE_SIZE: usize = 24;
pub const NODE_KEY_TAG: usize = 9;
pub const NODE_KEY_VALUE: usize = 16;

pub const STRING_SHORT_LENGTH: usize = 11;
pub const CLOSURE_COUNT: usize = 10;
pub const CLOSURE_PROTO: usize = 24;
pub const CLOSURE_VALUES: usize = 32;
pub const UPVALUE_POINTER: usize = 16;
pub const UPVALUE_NEXT: usize = 24;
pub const USER_COUNT: usize = 10;
pub const USER_LENGTH: usize = 16;
pub const USER_META: usize = 24;
pub const USER_VALUES: usize = 40;
pub const THREAD_TOP: usize = 16;
pub const THREAD_STACK_END: usize = 40;
pub const THREAD_STACK: usize = 48;
pub const THREAD_OPEN_UPVALUE: usize = 56;
pub const THREAD_NCI: usize = 12;

pub const PROTO_SIZE: u64 = 128;
pub const PROTO_ARRAYS: [(usize, usize, usize, &str); 7] = [
    (20, 56, 16, "constant"),
    (24, 64, 4, "code"),
    (32, 72, 8, "proto"),
    (16, 80, 16, "upvalue-name"),
    (28, 88, 1, "lineinfo"),
    (40, 96, 8, "abslineinfo"),
    (36, 104, 16, "local-name"),
];
pub const PROTO_SOURCE: usize = 112;
pub const PROTO_LINE: usize = 44;
pub const PROTO_UPVALUES: usize = 80;
pub const PROTO_UPVALUE_COUNT: usize = 16;

#[derive(Clone, Copy)]
pub struct HeapLayout {
    pub gc_state: usize,
    pub string_data: usize,
    pub string_length: usize,
    pub shared_age: Option<u8>,
}

impl HeapLayout {
    pub fn lua54() -> Self {
        Self {
            gc_state: 101,
            string_data: 24,
            string_length: 16,
            shared_age: None,
        }
    }
    // Skynet v1.8.0's fork removes global_State.seed and adds TString.id. The
    // following pointer fields retain their offsets because of alignment padding.
    pub fn skynet54() -> Self {
        Self {
            gc_state: 97,
            string_data: 32,
            string_length: 24,
            shared_age: Some(7),
        }
    }
}
