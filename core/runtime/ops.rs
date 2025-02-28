// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
use crate::ops::*;
use crate::OpResult;
use crate::PromiseId;
use anyhow::Error;
use futures::future::Either;
use futures::future::Future;
use futures::future::FutureExt;
use futures::task::noop_waker_ref;
use serde::Deserialize;
use serde_v8::from_v8;
use serde_v8::V8Sliceable;
use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::c_void;
use std::future::ready;
use std::mem::MaybeUninit;
use std::option::Option;
use std::task::Context;
use std::task::Poll;
use v8::WriteOptions;

/// The default string buffer size on the stack that prevents mallocs in some
/// string functions. Keep in mind that Windows only offers 1MB stacks by default,
/// so this is a limited resource!
pub const STRING_STACK_BUFFER_SIZE: usize = 1024 * 8;

#[inline]
pub fn queue_fast_async_op<R: serde::Serialize + 'static>(
  ctx: &OpCtx,
  promise_id: PromiseId,
  op: impl Future<Output = Result<R, Error>> + 'static,
) {
  RefCell::borrow(&ctx.state).tracker.track_async(ctx.id);
  let fut =
    op.map(|result| crate::_ops::to_op_result(ctx.get_error_class_fn, result));
  ctx
    .context_state
    .borrow_mut()
    .pending_ops
    .spawn(OpCall::new(ctx, promise_id, fut));
}

#[inline]
pub fn map_async_op1<R: serde::Serialize + 'static>(
  ctx: &OpCtx,
  op: impl Future<Output = Result<R, Error>> + 'static,
) -> impl Future<Output = OpResult> {
  RefCell::borrow(&ctx.state).tracker.track_async(ctx.id);
  op.map(|res| crate::_ops::to_op_result(ctx.get_error_class_fn, res))
}

#[inline]
pub fn map_async_op2<R: serde::Serialize + 'static>(
  ctx: &OpCtx,
  op: impl Future<Output = R> + 'static,
) -> impl Future<Output = OpResult> {
  let state = RefCell::borrow(&ctx.state);
  state.tracker.track_async(ctx.id);

  op.map(|res| OpResult::Ok(res.into()))
}

#[inline]
pub fn map_async_op3<R: serde::Serialize + 'static>(
  ctx: &OpCtx,
  op: Result<impl Future<Output = Result<R, Error>> + 'static, Error>,
) -> impl Future<Output = OpResult> {
  RefCell::borrow(&ctx.state).tracker.track_async(ctx.id);
  match op {
    Err(err) => Either::Left(ready(OpResult::Err(OpError::new(
      ctx.get_error_class_fn,
      err,
    )))),
    Ok(fut) => Either::Right(
      fut.map(|res| crate::_ops::to_op_result(ctx.get_error_class_fn, res)),
    ),
  }
}

#[inline]
pub fn map_async_op4<R: serde::Serialize + 'static>(
  ctx: &OpCtx,
  op: Result<impl Future<Output = R> + 'static, Error>,
) -> impl Future<Output = OpResult> {
  RefCell::borrow(&ctx.state).tracker.track_async(ctx.id);
  match op {
    Err(err) => Either::Left(ready(OpResult::Err(OpError::new(
      ctx.get_error_class_fn,
      err,
    )))),
    Ok(fut) => Either::Right(fut.map(|r| OpResult::Ok(r.into()))),
  }
}

pub fn queue_async_op<'s>(
  ctx: &OpCtx,
  scope: &'s mut v8::HandleScope,
  deferred: bool,
  promise_id: PromiseId,
  op: impl Future<Output = OpResult> + 'static,
) -> Option<v8::Local<'s, v8::Value>> {
  // An op's realm (as given by `OpCtx::realm_idx`) must match the realm in
  // which it is invoked. Otherwise, we might have cross-realm object exposure.
  // deno_core doesn't currently support such exposure, even though embedders
  // can cause them, so we panic in debug mode (since the check is expensive).
  // TODO(mmastrac): Restore this
  // debug_assert_eq!(
  //   runtime_state.borrow().context(ctx.realm_idx as usize, scope),
  //   Some(scope.get_current_context())
  // );

  let id = ctx.id;

  // TODO(mmastrac): We have to poll every future here because that assumption is baked into a large number
  // of ops. If we can figure out a way around this, we can remove this call to boxed_local and save a malloc per future.
  let mut pinned = op.map(move |res| (promise_id, id, res)).boxed_local();

  match pinned.poll_unpin(&mut Context::from_waker(noop_waker_ref())) {
    Poll::Pending => {}
    Poll::Ready(res) => {
      if deferred {
        ctx.context_state.borrow_mut().pending_ops.spawn(ready(res));
        return None;
      } else {
        ctx.state.borrow_mut().tracker.track_async_completed(ctx.id);
        return Some(res.2.to_v8(scope).unwrap());
      }
    }
  }

  ctx.context_state.borrow_mut().pending_ops.spawn(pinned);
  None
}

#[inline]
pub fn map_async_op_infallible<R: 'static>(
  ctx: &OpCtx,
  lazy: bool,
  promise_id: i32,
  op: impl Future<Output = R> + 'static,
  rv_map: for<'r> fn(
    &mut v8::HandleScope<'r>,
    R,
  ) -> Result<v8::Local<'r, v8::Value>, serde_v8::Error>,
) -> Option<R> {
  let id = ctx.id;
  ctx.state.borrow().tracker.track_async(id);

  if lazy {
    let mapper = move |r| {
      (
        promise_id,
        id,
        OpResult::Op2Temp(Box::new(move |scope| rv_map(scope, r))),
      )
    };
    ctx
      .context_state
      .borrow_mut()
      .pending_ops
      .spawn(op.map(mapper));
    return None;
  }

  // TODO(mmastrac): We have to poll every future here because that assumption is baked into a large number
  // of ops. If we can figure out a way around this, we can remove this call to boxed_local and save a malloc per future.
  let mut pinned = op.map(move |res| (promise_id, id, res)).boxed_local();

  match pinned.poll_unpin(&mut Context::from_waker(noop_waker_ref())) {
    Poll::Pending => {}
    Poll::Ready(res) => {
      ctx.state.borrow_mut().tracker.track_async_completed(ctx.id);
      return Some(res.2);
    }
  }

  ctx
    .context_state
    .borrow_mut()
    .pending_ops
    .spawn(pinned.map(move |r| {
      (
        r.0,
        r.1,
        OpResult::Op2Temp(Box::new(move |scope| rv_map(scope, r.2))),
      )
    }));
  None
}

#[inline]
pub fn map_async_op_fallible<R: 'static, E: Into<Error> + 'static>(
  ctx: &OpCtx,
  lazy: bool,
  promise_id: i32,
  op: impl Future<Output = Result<R, E>> + 'static,
  rv_map: for<'r> fn(
    &mut v8::HandleScope<'r>,
    R,
  ) -> Result<v8::Local<'r, v8::Value>, serde_v8::Error>,
) -> Option<Result<R, E>> {
  let id = ctx.id;
  ctx.state.borrow().tracker.track_async(id);

  if lazy {
    let get_class = ctx.get_error_class_fn;
    ctx.context_state.borrow_mut().pending_ops.spawn(op.map(
      move |r| match r {
        Ok(v) => (
          promise_id,
          id,
          OpResult::Op2Temp(Box::new(move |scope| rv_map(scope, v))),
        ),
        Err(err) => (
          promise_id,
          id,
          OpResult::Err(OpError::new(get_class, err.into())),
        ),
      },
    ));
    return None;
  }

  // TODO(mmastrac): We have to poll every future here because that assumption is baked into a large number
  // of ops. If we can figure out a way around this, we can remove this call to boxed_local and save a malloc per future.
  let mut pinned = op.map(move |res| (promise_id, id, res)).boxed_local();

  match pinned.poll_unpin(&mut Context::from_waker(noop_waker_ref())) {
    Poll::Pending => {}
    Poll::Ready(res) => {
      ctx.state.borrow_mut().tracker.track_async_completed(ctx.id);
      return Some(res.2);
    }
  }

  let get_class = ctx.get_error_class_fn;
  ctx.context_state.borrow_mut().pending_ops.spawn(pinned.map(
    move |r| match r.2 {
      Ok(v) => (
        r.0,
        r.1,
        OpResult::Op2Temp(Box::new(move |scope| rv_map(scope, v))),
      ),
      Err(err) => {
        (r.0, r.1, OpResult::Err(OpError::new(get_class, err.into())))
      }
    },
  ));
  None
}

macro_rules! try_number_some {
  ($n:ident $type:ident $is:ident) => {
    if $n.$is() {
      // SAFETY: v8 handles can be transmuted
      let n: &v8::$type = unsafe { std::mem::transmute($n) };
      return Some(n.value() as _);
    }
  };
}

macro_rules! try_bignum {
  ($n:ident $method:ident) => {
    if $n.is_big_int() {
      // SAFETY: v8 handles can be transmuted
      let $n: &v8::BigInt = unsafe { std::mem::transmute($n) };
      return Some($n.$method().0 as _);
    }
  };
}

pub fn to_u32_option(number: &v8::Value) -> Option<i32> {
  try_number_some!(number Integer is_uint32);
  try_number_some!(number Int32 is_int32);
  try_number_some!(number Number is_number);
  try_bignum!(number u64_value);
  None
}

pub fn to_i32_option(number: &v8::Value) -> Option<i32> {
  try_number_some!(number Uint32 is_uint32);
  try_number_some!(number Int32 is_int32);
  try_number_some!(number Number is_number);
  try_bignum!(number i64_value);
  None
}

pub fn to_u64_option(number: &v8::Value) -> Option<u64> {
  try_number_some!(number Integer is_uint32);
  try_number_some!(number Int32 is_int32);
  try_number_some!(number Number is_number);
  try_bignum!(number u64_value);
  None
}

pub fn to_i64_option(number: &v8::Value) -> Option<i64> {
  try_number_some!(number Integer is_uint32);
  try_number_some!(number Int32 is_int32);
  try_number_some!(number Number is_number);
  try_bignum!(number u64_value);
  None
}

pub fn to_f32_option(number: &v8::Value) -> Option<f32> {
  try_number_some!(number Number is_number);
  try_bignum!(number i64_value);
  None
}

pub fn to_f64_option(number: &v8::Value) -> Option<f64> {
  try_number_some!(number Number is_number);
  try_bignum!(number i64_value);
  None
}

pub fn to_external_option(external: &v8::Value) -> Option<*mut c_void> {
  if external.is_external() {
    // SAFETY: We know this is an external
    let external: &v8::External = unsafe { std::mem::transmute(external) };
    Some(external.value())
  } else {
    None
  }
}

/// Expands `inbuf` to `outbuf`, assuming that `outbuf` has at least 2x `input_length`.
#[inline(always)]
unsafe fn latin1_to_utf8(
  input_length: usize,
  inbuf: *const u8,
  outbuf: *mut u8,
) -> usize {
  let mut output = 0;
  let mut input = 0;
  while input < input_length {
    let char = *(inbuf.add(input));
    if char < 0x80 {
      *(outbuf.add(output)) = char;
      output += 1;
    } else {
      // Top two bits
      *(outbuf.add(output)) = (char >> 6) | 0b1100_0000;
      // Bottom six bits
      *(outbuf.add(output + 1)) = (char & 0b0011_1111) | 0b1000_0000;
      output += 2;
    }
    input += 1;
  }
  output
}

/// Converts a [`v8::fast_api::FastApiOneByteString`] to either an owned string, or a borrowed string, depending on whether it fits into the
/// provided buffer.
pub fn to_str_ptr<'a, const N: usize>(
  string: &'a mut v8::fast_api::FastApiOneByteString,
  buffer: &'a mut [MaybeUninit<u8>; N],
) -> Cow<'a, str> {
  let input_buf = string.as_bytes();

  // Per benchmarking results, it's faster to do this check than to copy latin-1 -> utf8
  if input_buf.is_ascii() {
    // SAFETY: We just checked that it was ASCII
    return Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(input_buf) });
  }

  let input_len = input_buf.len();
  let output_len = buffer.len();

  // We know that this string is full of either one or two-byte UTF-8 chars, so if it's < 1/2 of N we
  // can skip the ASCII check and just start copying.
  if input_len < N / 2 {
    debug_assert!(output_len >= input_len * 2);
    let buffer = buffer.as_mut_ptr() as *mut u8;

    let written =
      // SAFETY: We checked that buffer is at least 2x the size of input_buf
      unsafe { latin1_to_utf8(input_buf.len(), input_buf.as_ptr(), buffer) };

    debug_assert!(written <= output_len);

    let slice = std::ptr::slice_from_raw_parts(buffer, written);
    // SAFETY: We know it's valid UTF-8, so make a string
    Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(&*slice) })
  } else {
    // TODO(mmastrac): We could be smarter here about not allocating
    Cow::Owned(to_string_ptr(string))
  }
}

/// Converts a [`v8::fast_api::FastApiOneByteString`] to an owned string. May over-allocate to avoid
/// re-allocation.
pub fn to_string_ptr(string: &v8::fast_api::FastApiOneByteString) -> String {
  let input_buf = string.as_bytes();
  let capacity = input_buf.len() * 2;

  // SAFETY: We're allocating a buffer of 2x the input size, writing valid UTF-8, then turning that into a string
  unsafe {
    // Create an uninitialized buffer of `capacity` bytes. We need to be careful here to avoid
    // accidentally creating a slice of u8 which would be invalid.
    let layout = std::alloc::Layout::from_size_align(capacity, 1).unwrap();
    let out = std::alloc::alloc(layout);

    let written = latin1_to_utf8(input_buf.len(), input_buf.as_ptr(), out);

    debug_assert!(written <= capacity);
    // We know it's valid UTF-8, so make a string
    String::from_raw_parts(out, written, capacity)
  }
}

pub fn to_cow_byte_ptr(
  string: &v8::fast_api::FastApiOneByteString,
) -> Cow<[u8]> {
  string.as_bytes().into()
}

/// Converts a [`v8::Value`] to an owned string.
#[inline(always)]
pub fn to_string(scope: &mut v8::Isolate, string: &v8::Value) -> String {
  if !string.is_string() {
    return String::new();
  }

  let string: &v8::String = unsafe { std::mem::transmute(string) };
  string.to_rust_string_lossy(scope)
}

/// Converts a [`v8::String`] to either an owned string, or a borrowed string, depending on whether it fits into the
/// provided buffer.
#[inline(always)]
pub fn to_str<'a, const N: usize>(
  scope: &mut v8::Isolate,
  string: &v8::Value,
  buffer: &'a mut [MaybeUninit<u8>; N],
) -> Cow<'a, str> {
  if !string.is_string() {
    return Cow::Borrowed("");
  }

  // SAFETY: We checked is_string above
  let string: &v8::String = unsafe { std::mem::transmute(string) };

  string.to_rust_cow_lossy(scope, buffer)
}

#[inline(always)]
pub fn to_cow_one_byte(
  scope: &mut v8::Isolate,
  string: &v8::Value,
) -> Result<Cow<'static, [u8]>, &'static str> {
  if !string.is_string() {
    return Err("expected String");
  }

  // SAFETY: We checked is_string above
  let string: &v8::String = unsafe { std::mem::transmute(string) };

  let capacity = string.length();
  if capacity == 0 {
    return Ok(Cow::Borrowed(&[]));
  }

  if !string.is_onebyte() && !string.contains_only_onebyte() {
    return Err("expected one-byte String");
  }

  // Create an uninitialized buffer of `capacity` bytes. We need to be careful here to avoid
  // accidentally creating a slice of u8 which would be invalid.
  unsafe {
    let layout = std::alloc::Layout::from_size_align(capacity, 1).unwrap();
    let out = std::alloc::alloc(layout);

    // Write the buffer to a slice made from this uninitialized data
    {
      let buffer = std::slice::from_raw_parts_mut(out as _, capacity);
      string.write_one_byte_uninit(
        scope,
        buffer,
        0,
        WriteOptions::NO_NULL_TERMINATION,
      );
    }

    Ok(Vec::from_raw_parts(out, capacity, capacity).into())
  }
}

/// Converts from a raw [`v8::Value`] to the expected V8 data type.
#[inline(always)]
#[allow(clippy::result_unit_err)]
pub fn v8_try_convert<'a, T>(
  value: v8::Local<'a, v8::Value>,
) -> Result<v8::Local<'a, T>, ()>
where
  v8::Local<'a, T>: TryFrom<v8::Local<'a, v8::Value>>,
{
  v8::Local::<T>::try_from(value).map_err(drop)
}

/// Converts from a raw [`v8::Value`] to the expected V8 data type, wrapped in an [`Option`].
#[inline(always)]
#[allow(clippy::result_unit_err)]
pub fn v8_try_convert_option<'a, T>(
  value: v8::Local<'a, v8::Value>,
) -> Result<Option<v8::Local<'a, T>>, ()>
where
  v8::Local<'a, T>: TryFrom<v8::Local<'a, v8::Value>>,
{
  if value.is_null_or_undefined() {
    Ok(None)
  } else {
    Ok(Some(v8::Local::<T>::try_from(value).map_err(drop)?))
  }
}

pub fn serde_v8_to_rust<'a, T: Deserialize<'a>>(
  scope: &mut v8::HandleScope,
  input: v8::Local<v8::Value>,
) -> serde_v8::Result<T> {
  from_v8(scope, input)
}

/// Retrieve a [`serde_v8::V8Slice`] from a typed array in an [`v8::ArrayBufferView`].
pub fn to_v8_slice<'a, T>(
  scope: &mut v8::HandleScope,
  input: v8::Local<'a, v8::Value>,
) -> Result<serde_v8::V8Slice<T>, &'static str>
where
  T: V8Sliceable,
  v8::Local<'a, T::V8>: TryFrom<v8::Local<'a, v8::Value>>,
  v8::Local<'a, v8::ArrayBufferView>: From<v8::Local<'a, T::V8>>,
{
  let (store, offset, length) =
    if let Ok(buf) = v8::Local::<T::V8>::try_from(input) {
      let buf: v8::Local<v8::ArrayBufferView> = buf.into();
      let Some(buffer) = buf.buffer(scope) else {
      return Err("buffer missing");
    };
      (
        buffer.get_backing_store(),
        buf.byte_offset(),
        buf.byte_length(),
      )
    } else {
      return Err("expected typed ArrayBufferView");
    };
  let slice =
    unsafe { serde_v8::V8Slice::from_parts(store, offset..(offset + length)) };
  Ok(slice)
}

/// Retrieve a [`serde_v8::V8Slice`] from a typed array in an [`v8::ArrayBufferView`].
pub fn to_v8_slice_detachable<'a, T>(
  scope: &mut v8::HandleScope,
  input: v8::Local<'a, v8::Value>,
) -> Result<serde_v8::V8Slice<T>, &'static str>
where
  T: V8Sliceable,
  v8::Local<'a, T::V8>: TryFrom<v8::Local<'a, v8::Value>>,
  v8::Local<'a, v8::ArrayBufferView>: From<v8::Local<'a, T::V8>>,
{
  let (store, offset, length) =
    if let Ok(buf) = v8::Local::<T::V8>::try_from(input) {
      let buf: v8::Local<v8::ArrayBufferView> = buf.into();
      let Some(buffer) = buf.buffer(scope) else {
      return Err("buffer missing");
    };
      let res = (
        buffer.get_backing_store(),
        buf.byte_offset(),
        buf.byte_length(),
      );
      if !buffer.is_detachable() {
        return Err("invalid type; expected: detachable");
      }
      buffer.detach(None);
      res
    } else {
      return Err("expected typed ArrayBufferView");
    };
  let slice =
    unsafe { serde_v8::V8Slice::from_parts(store, offset..(offset + length)) };
  Ok(slice)
}

#[cfg(test)]
mod tests {
  use crate::error::generic_error;
  use crate::error::AnyError;
  use crate::error::JsError;
  use crate::FastString;
  use crate::JsRuntime;
  use crate::OpState;
  use crate::RuntimeOptions;
  use anyhow::bail;
  use anyhow::Error;
  use bytes::BytesMut;
  use deno_ops::op2;
  use futures::Future;
  use serde::Deserialize;
  use serde::Serialize;
  use serde_v8::JsBuffer;
  use std::borrow::Cow;
  use std::cell::Cell;
  use std::cell::RefCell;
  use std::rc::Rc;
  use std::time::Duration;

  crate::extension!(
    testing,
    ops = [
      op_test_fail,
      op_test_print_debug,

      op_test_add,
      op_test_add_smi_unsigned,
      op_test_add_option,
      op_test_result_void_switch,
      op_test_result_void_ok,
      op_test_result_void_err,
      op_test_result_primitive_ok,
      op_test_result_primitive_err,
      op_test_bool,
      op_test_bool_result,
      op_test_float,
      op_test_float_result,
      op_test_bigint_i64,
      op_test_bigint_i64_as_number,
      op_test_bigint_u64,
      op_test_string_owned,
      op_test_string_ref,
      op_test_string_cow,
      op_test_string_roundtrip_char,
      op_test_string_roundtrip_char_onebyte,
      op_test_string_return,
      op_test_string_option_return,
      op_test_string_roundtrip,
      op_test_string_roundtrip_onebyte,
      op_test_generics<String>,
      op_test_v8_types,
      op_test_v8_option_string,
      op_test_v8_type_return,
      op_test_v8_type_return_option,
      op_test_v8_type_handle_scope,
      op_test_v8_type_handle_scope_obj,
      op_test_v8_type_handle_scope_result,
      op_test_v8_global,
      op_test_serde_v8,
      op_state_rc,
      op_state_ref,
      op_state_mut,
      op_state_mut_attr,
      op_state_multi_attr,
      op_buffer_slice,
      op_buffer_slice_32,
      op_buffer_slice_unsafe_callback,
      op_buffer_copy,
      op_buffer_bytesmut,
      op_external_make,
      op_external_process,

      op_async_void,
      op_async_number,
      op_async_add,
      op_async_add_smi,
      op_async_sleep,
      op_async_sleep_impl,
      op_async_sleep_error,
      op_async_lazy_error,
      op_async_result_impl,
      op_async_state_rc,
      op_async_buffer,
      op_async_buffer_vec,
      op_async_buffer_impl,
      op_async_external,
      op_async_serde_option_v8,
    ],
    state = |state| {
      state.put(1234u32);
      state.put(10000u16);
    }
  );

  thread_local! {
    static FAIL: Cell<bool> = Cell::new(false)
  }

  #[op2(core, fast)]
  pub fn op_test_fail() {
    FAIL.with(|b| b.set(true))
  }

  #[op2(core, fast)]
  pub fn op_test_print_debug(#[string] s: &str) {
    println!("{s}")
  }

  /// Run a test for a single op.
  fn run_test2(repeat: usize, op: &str, test: &str) -> Result<(), AnyError> {
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![testing::init_ops_and_esm()],
      ..Default::default()
    });
    let err_mapper =
      |err| generic_error(format!("{op} test failed ({test}): {err:?}"));
    runtime
      .execute_script(
        "",
        FastString::Owned(
          format!(
            r"
            const {{ op_test_fail, op_test_print_debug, {op} }} = Deno.core.ensureFastOps();
            function assert(b) {{
              if (!b) {{
                op_test_fail();
              }}
            }}
            function assertErrorContains(e, s) {{
              assert(String(e).indexOf(s) != -1)
            }}
            function log(s) {{
              op_test_print_debug(String(s))
            }}
          "
          )
          .into(),
        ),
      )
      .map_err(err_mapper)?;
    FAIL.with(|b| b.set(false));
    runtime.execute_script(
      "",
      FastString::Owned(
        format!(
          r"
      for (let __index__ = 0; __index__ < {repeat}; __index__++) {{
        {test}
      }}
    "
        )
        .into(),
      ),
    )?;
    if FAIL.with(|b| b.get()) {
      Err(generic_error(format!("{op} test failed ({test})")))
    } else {
      Ok(())
    }
  }

  /// Run a test for a single op.
  async fn run_async_test(
    repeat: usize,
    op: &str,
    test: &str,
  ) -> Result<(), AnyError> {
    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![testing::init_ops_and_esm()],
      ..Default::default()
    });
    let err_mapper =
      |err| generic_error(format!("{op} test failed ({test}): {err:?}"));
    runtime
      .execute_script(
        "",
        FastString::Owned(
          format!(
            r"
            const {{ op_test_fail, op_test_print_debug, {op} }} = Deno.core.ensureFastOps();
            function assert(b) {{
              if (!b) {{
                op_test_fail();
              }}
            }}
            function assertErrorContains(e, s) {{
              assert(String(e).indexOf(s) != -1)
            }}
            function log(s) {{
              op_test_print_debug(String(s))
            }}
          "
          )
          .into(),
        ),
      )
      .map_err(err_mapper)?;
    FAIL.with(|b| b.set(false));
    runtime.execute_script(
      "",
      FastString::Owned(
        format!(
          r"
            (async () => {{
              for (let __index__ = 0; __index__ < {repeat}; __index__++) {{
                {test}
              }}
            }})()
          "
        )
        .into(),
      ),
    )?;

    runtime.run_event_loop(false).await?;
    if FAIL.with(|b| b.get()) {
      Err(generic_error(format!("{op} test failed ({test})")))
    } else {
      Ok(())
    }
  }

  #[tokio::test(flavor = "current_thread")]
  pub async fn test_op_fail() {
    assert!(run_test2(1, "", "assert(false)").is_err());
  }

  #[op2(core, fast)]
  pub fn op_test_add(a: u32, b: i32) -> u32 {
    (a as i32 + b) as u32
  }

  /// Test various numeric coercions in fast and slow mode.
  #[tokio::test(flavor = "current_thread")]
  pub async fn test_op_add() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(10000, "op_test_add", "assert(op_test_add(1, 11) == 12)")?;
    run_test2(10000, "op_test_add", "assert(op_test_add(11, -1) == 10)")?;
    run_test2(10000, "op_test_add", "assert(op_test_add(1.5, 11.5) == 12)")?;
    run_test2(10000, "op_test_add", "assert(op_test_add(11.5, -1) == 10)")?;
    run_test2(
      10000,
      "op_test_add",
      "assert(op_test_add(4096n, 4096n) == 4096 + 4096)",
    )?;
    run_test2(
      10000,
      "op_test_add",
      "assert(op_test_add(8192n, -4096n) == 4096)",
    )?;
    Ok(())
  }

  // Note: #[smi] parameters are signed in JS regardless of the sign in Rust. Overflow and underflow
  // of valid ranges result in automatic wrapping.
  #[op2(core, fast)]
  #[smi]
  pub fn op_test_add_smi_unsigned(#[smi] a: u32, #[smi] b: u16) -> u32 {
    a + b as u32
  }

  /// Test various numeric coercions in fast and slow mode.
  #[tokio::test(flavor = "current_thread")]
  pub async fn test_op_add_smi() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10000,
      "op_test_add_smi_unsigned",
      "assert(op_test_add_smi_unsigned(1000, 2000) == 3000)",
    )?;
    run_test2(
      10000,
      "op_test_add_smi_unsigned",
      "assert(op_test_add_smi_unsigned(-1000, 10) == -990)",
    )?;
    Ok(())
  }

  #[op2(core)]
  pub fn op_test_add_option(a: u32, b: Option<u32>) -> u32 {
    a + b.unwrap_or(100)
  }

  #[tokio::test(flavor = "current_thread")]
  pub async fn test_op_add_option() -> Result<(), Box<dyn std::error::Error>> {
    // This isn't fast, so we don't repeat it
    run_test2(
      1,
      "op_test_add_option",
      "assert(op_test_add_option(1, 11) == 12)",
    )?;
    run_test2(
      1,
      "op_test_add_option",
      "assert(op_test_add_option(1, null) == 101)",
    )?;
    Ok(())
  }

  thread_local! {
    static RETURN_COUNT: Cell<usize> = Cell::new(0);
  }

  #[op2(core, fast)]
  pub fn op_test_result_void_switch() -> Result<(), AnyError> {
    let count = RETURN_COUNT.with(|count| {
      let new = count.get() + 1;
      count.set(new);
      new
    });
    if count > 5000 {
      Err(generic_error("failed!!!"))
    } else {
      Ok(())
    }
  }

  #[op2(core, fast)]
  pub fn op_test_result_void_err() -> Result<(), AnyError> {
    Err(generic_error("failed!!!"))
  }

  #[op2(core, fast)]
  pub fn op_test_result_void_ok() -> Result<(), AnyError> {
    Ok(())
  }

  #[tokio::test(flavor = "current_thread")]
  pub async fn test_op_result_void() -> Result<(), Box<dyn std::error::Error>> {
    // Test the non-switching kinds
    run_test2(
      10000,
      "op_test_result_void_err",
      "try { op_test_result_void_err(); assert(false) } catch (e) {}",
    )?;
    run_test2(10000, "op_test_result_void_ok", "op_test_result_void_ok()")?;
    Ok(())
  }

  #[tokio::test(flavor = "current_thread")]
  pub async fn test_op_result_void_switch(
  ) -> Result<(), Box<dyn std::error::Error>> {
    RETURN_COUNT.with(|count| count.set(0));
    let err = run_test2(
      10000,
      "op_test_result_void_switch",
      "op_test_result_void_switch();",
    )
    .expect_err("Expected this to fail");
    let js_err = err.downcast::<JsError>().unwrap();
    assert_eq!(js_err.message, Some("failed!!!".into()));
    assert_eq!(RETURN_COUNT.with(|count| count.get()), 5001);
    Ok(())
  }

  #[op2(core, fast)]
  pub fn op_test_result_primitive_err() -> Result<u32, AnyError> {
    Err(generic_error("failed!!!"))
  }

  #[op2(core, fast)]
  pub fn op_test_result_primitive_ok() -> Result<u32, AnyError> {
    Ok(123)
  }

  #[tokio::test]
  pub async fn test_op_result_primitive(
  ) -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10000,
      "op_test_result_primitive_err",
      "try { op_test_result_primitive_err(); assert(false) } catch (e) {}",
    )?;
    run_test2(
      10000,
      "op_test_result_primitive_ok",
      "op_test_result_primitive_ok()",
    )?;
    Ok(())
  }

  #[op2(core, fast)]
  pub fn op_test_bool(b: bool) -> bool {
    b
  }

  #[op2(core, fast)]
  pub fn op_test_bool_result(b: bool) -> Result<bool, AnyError> {
    if b {
      Ok(true)
    } else {
      Err(generic_error("false!!!"))
    }
  }

  #[tokio::test]
  pub async fn test_op_bool() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10000,
      "op_test_bool",
      "assert(op_test_bool(true) === true && op_test_bool(false) === false)",
    )?;
    run_test2(
      10000,
      "op_test_bool_result",
      "assert(op_test_bool_result(true) === true)",
    )?;
    run_test2(
      1,
      "op_test_bool_result",
      "try { op_test_bool_result(false); assert(false) } catch (e) {}",
    )?;
    Ok(())
  }

  #[op2(core, fast)]
  pub fn op_test_float(a: f32, b: f64) -> f32 {
    a + b as f32
  }

  #[op2(core, fast)]
  pub fn op_test_float_result(a: f32, b: f64) -> Result<f64, AnyError> {
    let a = a as f64;
    if a + b >= 0. {
      Ok(a + b)
    } else {
      Err(generic_error("negative!!!"))
    }
  }

  #[tokio::test]
  pub async fn test_op_float() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(10000, "op_test_float", "assert(op_test_float(1, 10) == 11)")?;
    run_test2(
      10000,
      "op_test_float_result",
      "assert(op_test_float_result(1, 10) == 11)",
    )?;
    run_test2(
      1,
      "op_test_float_result",
      "try { op_test_float_result(-1, -1); assert(false) } catch (e) {}",
    )?;
    Ok(())
  }

  #[op2(core)]
  #[bigint]
  pub fn op_test_bigint_u64(#[bigint] input: u64) -> u64 {
    input
  }

  #[op2(core)]
  #[bigint]
  pub fn op_test_bigint_i64(#[bigint] input: i64) -> i64 {
    input
  }

  #[op2(core, fast)]
  #[number]
  pub fn op_test_bigint_i64_as_number(#[number] input: i64) -> i64 {
    input
  }

  #[tokio::test]
  pub async fn test_op_64() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10,
      "op_test_bigint_i64",
      &format!("assert(op_test_bigint_i64({}n) == {}n)", i64::MAX, i64::MAX),
    )?;
    run_test2(
      10000,
      "op_test_bigint_i64_as_number",
      "assert(op_test_bigint_i64_as_number(Number.MAX_SAFE_INTEGER) == Number.MAX_SAFE_INTEGER)",
    )?;
    run_test2(
      10000,
      "op_test_bigint_i64_as_number",
      "assert(op_test_bigint_i64_as_number(Number.MIN_SAFE_INTEGER) == Number.MIN_SAFE_INTEGER)",
    )?;
    run_test2(
      10,
      "op_test_bigint_i64",
      &format!("assert(op_test_bigint_i64({}n) == {}n)", i64::MIN, i64::MIN),
    )?;
    run_test2(
      10,
      "op_test_bigint_u64",
      &format!("assert(op_test_bigint_u64({}n) == {}n)", u64::MAX, u64::MAX),
    )?;
    Ok(())
  }

  #[op2(core, fast)]
  pub fn op_test_string_owned(#[string] s: String) -> u32 {
    s.len() as _
  }

  #[op2(core, fast)]
  pub fn op_test_string_ref(#[string] s: &str) -> u32 {
    s.len() as _
  }

  #[op2(core, fast)]
  pub fn op_test_string_cow(#[string] s: Cow<str>) -> u32 {
    s.len() as _
  }

  #[op2(core, fast)]
  pub fn op_test_string_roundtrip_char(#[string] s: Cow<str>) -> u32 {
    s.chars().next().unwrap() as u32
  }

  #[op2(core, fast)]
  pub fn op_test_string_roundtrip_char_onebyte(
    #[string(onebyte)] s: Cow<[u8]>,
  ) -> u32 {
    s[0] as u32
  }

  #[tokio::test]
  pub async fn test_op_strings() -> Result<(), Box<dyn std::error::Error>> {
    for op in [
      "op_test_string_owned",
      "op_test_string_cow",
      "op_test_string_ref",
    ] {
      for (len, str) in [
        // ASCII
        (3, "'abc'"),
        // Latin-1 (one byte but two UTF-8 chars)
        (2, "'\\u00a0'"),
        // ASCII
        (1000, "'a'.repeat(1000)"),
        // Latin-1
        (2000, "'\\u00a0'.repeat(1000)"),
        // 4-byte UTF-8 emoji (1F995 = 🦕)
        (4000, "'\\u{1F995}'.repeat(1000)"),
        // ASCII
        (10000, "'a'.repeat(10000)"),
        // Latin-1
        (20000, "'\\u00a0'.repeat(10000)"),
        // 4-byte UTF-8 emoji (1F995 = 🦕)
        (40000, "'\\u{1F995}'.repeat(10000)"),
      ] {
        let test = format!("assert({op}({str}) == {len})");
        run_test2(10000, op, &test)?;
      }
    }

    // Ensure that we're correctly encoding UTF-8
    run_test2(
      10000,
      "op_test_string_roundtrip_char",
      "assert(op_test_string_roundtrip_char('\\u00a0') == 0xa0)",
    )?;
    run_test2(
      10000,
      "op_test_string_roundtrip_char",
      "assert(op_test_string_roundtrip_char('\\u00ff') == 0xff)",
    )?;
    run_test2(
      10000,
      "op_test_string_roundtrip_char",
      "assert(op_test_string_roundtrip_char('\\u0080') == 0x80)",
    )?;
    run_test2(
      10000,
      "op_test_string_roundtrip_char",
      "assert(op_test_string_roundtrip_char('\\u0100') == 0x100)",
    )?;

    run_test2(
      10000,
      "op_test_string_roundtrip_char_onebyte",
      "assert(op_test_string_roundtrip_char_onebyte('\\u00ff') == 0xff)",
    )?;
    run_test2(
      10000,
      "op_test_string_roundtrip_char_onebyte",
      "assert(op_test_string_roundtrip_char_onebyte('\\u007f') == 0x7f)",
    )?;
    run_test2(
      10,
      "op_test_string_roundtrip_char_onebyte",
      "try { op_test_string_roundtrip_char_onebyte('\\u1000'); assert(false); } catch (e) {}"
    )?;

    Ok(())
  }

  #[op2(core)]
  #[string]
  pub fn op_test_string_return(
    #[string] a: Cow<str>,
    #[string] b: Cow<str>,
  ) -> String {
    (a + b).to_string()
  }

  #[op2(core)]
  #[string]
  pub fn op_test_string_option_return(
    #[string] a: Cow<str>,
    #[string] b: Cow<str>,
  ) -> Option<String> {
    if a == "none" {
      return None;
    }
    Some((a + b).to_string())
  }

  #[op2(core)]
  #[string]
  pub fn op_test_string_roundtrip(#[string] s: String) -> String {
    s
  }

  #[op2(core)]
  #[string(onebyte)]
  pub fn op_test_string_roundtrip_onebyte(
    #[string(onebyte)] s: Cow<[u8]>,
  ) -> Cow<[u8]> {
    s
  }

  #[tokio::test]
  pub async fn test_op_string_returns() -> Result<(), Box<dyn std::error::Error>>
  {
    run_test2(
      1,
      "op_test_string_return",
      "assert(op_test_string_return('a', 'b') == 'ab')",
    )?;
    run_test2(
      1,
      "op_test_string_option_return",
      "assert(op_test_string_option_return('a', 'b') == 'ab')",
    )?;
    run_test2(
      1,
      "op_test_string_option_return",
      "assert(op_test_string_option_return('none', 'b') == null)",
    )?;
    run_test2(
      1,
      "op_test_string_roundtrip",
      "assert(op_test_string_roundtrip('\\u0080\\u00a0\\u00ff') == '\\u0080\\u00a0\\u00ff')",
    )?;
    run_test2(
      1,
      "op_test_string_roundtrip_onebyte",
      "assert(op_test_string_roundtrip_onebyte('\\u0080\\u00a0\\u00ff') == '\\u0080\\u00a0\\u00ff')",
    )?;
    Ok(())
  }

  // We don't actually test this one -- we just want it to compile
  #[op2(core, fast)]
  pub fn op_test_generics<T: Clone>() {}

  /// Tests v8 types without a handle scope
  #[allow(clippy::needless_lifetimes)]
  #[op2(core, fast)]
  pub fn op_test_v8_types<'s>(
    s: &v8::String,
    s2: v8::Local<v8::String>,
    s3: v8::Local<'s, v8::String>,
  ) -> u32 {
    if s.same_value(s2.into()) {
      1
    } else if s.same_value(s3.into()) {
      2
    } else {
      3
    }
  }

  #[op2(core, fast)]
  pub fn op_test_v8_option_string(s: Option<&v8::String>) -> i32 {
    if let Some(s) = s {
      s.length() as i32
    } else {
      -1
    }
  }

  /// Tests v8 types without a handle scope
  #[op2(core)]
  #[allow(clippy::needless_lifetimes)]
  pub fn op_test_v8_type_return<'s>(
    s: v8::Local<'s, v8::String>,
  ) -> v8::Local<'s, v8::String> {
    s
  }

  /// Tests v8 types without a handle scope
  #[op2(core)]
  #[allow(clippy::needless_lifetimes)]
  pub fn op_test_v8_type_return_option<'s>(
    s: Option<v8::Local<'s, v8::String>>,
  ) -> Option<v8::Local<'s, v8::String>> {
    s
  }

  #[op2(core)]
  pub fn op_test_v8_type_handle_scope<'s>(
    scope: &mut v8::HandleScope<'s>,
    s: &v8::String,
  ) -> v8::Local<'s, v8::String> {
    let s = s.to_rust_string_lossy(scope);
    v8::String::new(scope, &s).unwrap()
  }

  /// Extract whatever lives in "key" from the object.
  #[op2(core)]
  pub fn op_test_v8_type_handle_scope_obj<'s>(
    scope: &mut v8::HandleScope<'s>,
    o: &v8::Object,
  ) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, "key").unwrap().into();
    o.get(scope, key)
  }

  /// Extract whatever lives in "key" from the object.
  #[op2(core)]
  pub fn op_test_v8_type_handle_scope_result<'s>(
    scope: &mut v8::HandleScope<'s>,
    o: &v8::Object,
  ) -> Result<v8::Local<'s, v8::Value>, AnyError> {
    let key = v8::String::new(scope, "key").unwrap().into();
    o.get(scope, key)
      .filter(|v| !v.is_null_or_undefined())
      .ok_or(generic_error("error!!!"))
  }

  #[tokio::test]
  pub async fn test_op_v8_types() -> Result<(), Box<dyn std::error::Error>> {
    for (a, b) in [("a", 1), ("b", 2), ("c", 3)] {
      run_test2(
        10000,
        "op_test_v8_types",
        &format!("assert(op_test_v8_types('{a}', 'a', 'b') == {b})"),
      )?;
    }
    // Fast ops
    for (a, b, c) in [
      ("op_test_v8_option_string", "'xyz'", "3"),
      ("op_test_v8_option_string", "null", "-1"),
    ] {
      run_test2(10000, a, &format!("assert({a}({b}) == {c})"))?;
    }
    // Non-fast ops
    for (a, b, c) in [
      ("op_test_v8_type_return", "'xyz'", "'xyz'"),
      ("op_test_v8_type_return_option", "'xyz'", "'xyz'"),
      ("op_test_v8_type_return_option", "null", "null"),
      ("op_test_v8_type_handle_scope", "'xyz'", "'xyz'"),
      ("op_test_v8_type_handle_scope_obj", "{'key': 1}", "1"),
      (
        "op_test_v8_type_handle_scope_obj",
        "{'key': 'abc'}",
        "'abc'",
      ),
      (
        "op_test_v8_type_handle_scope_obj",
        "{'no_key': 'abc'}",
        "null",
      ),
      (
        "op_test_v8_type_handle_scope_result",
        "{'key': 'abc'}",
        "'abc'",
      ),
    ] {
      run_test2(1, a, &format!("assert({a}({b}) == {c})"))?;
    }

    // Test the error case for op_test_v8_type_handle_scope_result
    run_test2(1, "op_test_v8_type_handle_scope_result", "try { op_test_v8_type_handle_scope_result({}); assert(false); } catch (e) {}")?;
    Ok(())
  }

  #[op2(core)]
  pub fn op_test_v8_global(
    scope: &mut v8::HandleScope,
    #[global] s: v8::Global<v8::String>,
  ) -> u32 {
    let s = s.open(scope);
    s.length() as _
  }

  #[tokio::test]
  pub async fn test_op_v8_global() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      1,
      "op_test_v8_global",
      "assert(op_test_v8_global('hello world') == 11)",
    )?;
    Ok(())
  }

  #[derive(Serialize, Deserialize)]
  pub struct Serde {
    pub s: String,
  }

  #[op2(core)]
  #[serde]
  pub fn op_test_serde_v8(#[serde] mut serde: Serde) -> Serde {
    serde.s += "!";
    serde
  }

  #[tokio::test]
  pub async fn test_op_serde_v8() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      1,
      "op_test_serde_v8",
      "assert(op_test_serde_v8({s: 'abc'}).s == 'abc!')",
    )?;
    run_test2(
      1,
      "op_test_serde_v8",
      "try { op_test_serde_v8({}); assert(false) } catch (e) { assertErrorContains(e, 'missing field') }",
    )?;
    Ok(())
  }

  #[op2(core, fast)]
  pub fn op_state_rc(state: Rc<RefCell<OpState>>, value: u32) -> u32 {
    let old_value: u32 = state.borrow_mut().take();
    state.borrow_mut().put(value);
    old_value
  }

  #[op2(core, fast)]
  pub fn op_state_ref(state: &OpState) -> u32 {
    let old_value: &u32 = state.borrow();
    *old_value
  }

  #[op2(core, fast)]
  pub fn op_state_mut(state: &mut OpState, value: u32) {
    *state.borrow_mut() = value;
  }

  #[op2(core, fast)]
  pub fn op_state_mut_attr(#[state] value: &mut u32, new_value: u32) -> u32 {
    let old_value = *value;
    *value = new_value;
    old_value
  }

  #[op2(core, fast)]
  pub fn op_state_multi_attr(
    #[state] value32: &u32,
    #[state] value16: &u16,
    #[state] value8: Option<&u8>,
  ) -> u32 {
    assert_eq!(value8, None);
    *value32 + *value16 as u32
  }

  #[tokio::test]
  pub async fn test_op_state() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10000,
      "op_state_rc",
      "if (__index__ == 0) { op_state_rc(__index__) } else { assert(op_state_rc(__index__) == __index__ - 1) }",
    )?;
    run_test2(
      10000,
      "op_state_mut_attr",
      "if (__index__ == 0) { op_state_mut_attr(__index__) } else { assert(op_state_mut_attr(__index__) == __index__ - 1) }",
    )?;
    run_test2(10000, "op_state_mut", "op_state_mut(__index__)")?;
    run_test2(10000, "op_state_ref", "assert(op_state_ref() == 1234)")?;
    run_test2(
      10000,
      "op_state_multi_attr",
      "assert(op_state_multi_attr() == 11234)",
    )?;
    Ok(())
  }

  #[op2(core, fast)]
  pub fn op_buffer_slice(
    #[buffer] input: &[u8],
    #[number] inlen: usize,
    #[buffer] output: &mut [u8],
    #[number] outlen: usize,
  ) {
    assert_eq!(inlen, input.len());
    assert_eq!(outlen, output.len());
    output[0] = input[0];
  }

  #[op2(core, fast)]
  pub fn op_buffer_slice_32(
    #[buffer] input: &[u32],
    #[number] inlen: usize,
    #[buffer] output: &mut [u32],
    #[number] outlen: usize,
  ) {
    assert_eq!(inlen, input.len());
    assert_eq!(outlen, output.len());
    output[0] = input[0];
  }

  #[tokio::test]
  pub async fn test_op_buffer_slice() -> Result<(), Box<dyn std::error::Error>>
  {
    for (op, arr, size) in [
      ("op_buffer_slice", "Uint8Array", 1),
      ("op_buffer_slice_32", "Uint32Array", 4),
    ] {
      // UintXArray -> UintXArray
      run_test2(
        10000,
        op,
        &format!(
          r"
        let out = new {arr}(10);
        {op}(new {arr}([1,2,3]), 3, out, 10);
        assert(out[0] == 1);"
        ),
      )?;
      // UintXArray(ArrayBuffer) -> UintXArray(ArrayBuffer)
      run_test2(
        10000,
        op,
        &format!(
          r"
        let inbuf = new ArrayBuffer(10 * {size});
        let in_u8 = new {arr}(inbuf);
        in_u8[0] = 1;
        let out = new ArrayBuffer(10 * {size});
        {op}(in_u8, 10, new {arr}(out), 10);
        assert(new {arr}(out)[0] == 1);"
        ),
      )?;
      // UintXArray(ArrayBuffer, 5, 5) -> UintXArray(ArrayBuffer)
      run_test2(
        10000,
        op,
        &format!(
          r"
        let inbuf = new ArrayBuffer(10 * {size});
        let in_u8 = new {arr}(inbuf);
        in_u8[5] = 1;
        let out = new ArrayBuffer(10 * {size});
        {op}(new {arr}(inbuf, 5 * {size}, 5), 5, new {arr}(out), 10);
        assert(new {arr}(out)[0] == 1);"
        ),
      )?;
      // Resizable
      run_test2(
        10000,
        op,
        &format!(
          r"
        let inbuf = new ArrayBuffer(10 * {size}, {{ maxByteLength: 100 * {size} }});
        let in_u8 = new {arr}(inbuf);
        in_u8[5] = 1;
        let out = new ArrayBuffer(10 * {size}, {{ maxByteLength: 100 * {size} }});
        {op}(new {arr}(inbuf, 5 * {size}, 5), 5, new {arr}(out), 10);
        assert(new {arr}(out)[0] == 1);"
        ),
      )?;
    }
    Ok(())
  }

  // TODO(mmastrac): This is a dangerous op that we'll use to test resizable buffers in a later pass.
  #[op2(core)]
  pub fn op_buffer_slice_unsafe_callback(
    scope: &mut v8::HandleScope,
    buffer: v8::Local<v8::ArrayBuffer>,
    callback: v8::Local<v8::Function>,
  ) {
    println!("{:?}", buffer.data());
    let recv = callback.into();
    callback.call(scope, recv, &[]);
    println!("{:?}", buffer.data());
  }

  #[ignore]
  #[tokio::test]
  async fn test_op_unsafe() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      1,
      "op_buffer_slice_unsafe_callback",
      r"
      let inbuf = new ArrayBuffer(1024 * 1024, { maxByteLength: 10 * 1024 * 1024 });
      op_buffer_slice_unsafe_callback(inbuf, () => {
        inbuf.resize(0);
      });
      ",
    )?;
    Ok(())
  }

  /// Ensures that three copies are independent. Note that we cannot mutate the
  /// `bytes::Bytes`.
  #[op2(core, fast)]
  #[allow(clippy::boxed_local)] // Clippy bug? It warns about input2
  pub fn op_buffer_copy(
    #[buffer(copy)] mut input1: Vec<u8>,
    #[buffer(copy)] mut input2: Box<[u8]>,
    #[buffer(copy)] input3: bytes::Bytes,
  ) {
    assert_eq!(input1[0], input2[0]);
    assert_eq!(input2[0], input3[0]);
    input1[0] = 0xff;
    assert_ne!(input1[0], input2[0]);
    assert_eq!(input2[0], input3[0]);
    input2[0] = 0xff;
    assert_eq!(input1[0], input2[0]);
    assert_ne!(input2[0], input3[0]);
  }

  #[tokio::test]
  pub async fn test_op_buffer_copy() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10000,
      "op_buffer_copy",
      r"
      let input = new Uint8Array(10);
      input[0] = 1;
      op_buffer_copy(input, input, input);
      assert(input[0] == 1);",
    )?;
    Ok(())
  }

  #[op2(core)]
  #[buffer]
  pub fn op_buffer_bytesmut() -> BytesMut {
    let mut buffer = BytesMut::new();
    buffer.extend_from_slice(&[1, 2, 3]);
    buffer
  }

  #[tokio::test]
  pub async fn test_op_buffer_bytesmut(
  ) -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10,
      "op_buffer_bytesmut",
      r"
      const array = op_buffer_bytesmut();
      assert(array.length == 3);",
    )?;
    Ok(())
  }

  static STRING: &str = "hello world";

  #[op2(core, fast)]
  fn op_external_make() -> *const std::ffi::c_void {
    STRING.as_ptr() as _
  }

  #[op2(core, fast)]
  fn op_external_process(
    input: *const std::ffi::c_void,
  ) -> *const std::ffi::c_void {
    assert_eq!(input, STRING.as_ptr() as _);
    input
  }

  #[tokio::test]
  pub async fn test_external() -> Result<(), Box<dyn std::error::Error>> {
    run_test2(
      10000,
      "op_external_make, op_external_process",
      "op_external_process(op_external_make())",
    )?;
    Ok(())
  }

  #[op2(async, core)]
  async fn op_async_void() {}

  #[tokio::test]
  pub async fn test_op_async_void() -> Result<(), Box<dyn std::error::Error>> {
    run_async_test(10000, "op_async_void", "await op_async_void()").await?;
    Ok(())
  }

  #[op2(async, core)]
  async fn op_async_number(x: u32) -> u32 {
    x
  }

  #[op2(async, core)]
  async fn op_async_add(x: u32, y: u32) -> u32 {
    x.wrapping_add(y)
  }

  // Note: #[smi] parameters are signed in JS regardless of the sign in Rust. Overflow and underflow
  // of valid ranges result in automatic wrapping.
  #[op2(async, core)]
  #[smi]
  async fn op_async_add_smi(#[smi] x: u32, #[smi] y: u32) -> u32 {
    tokio::time::sleep(Duration::from_millis(10)).await;
    x.wrapping_add(y)
  }

  #[tokio::test]
  pub async fn test_op_async_number() -> Result<(), Box<dyn std::error::Error>>
  {
    run_async_test(
      10000,
      "op_async_number",
      "assert(await op_async_number(__index__) == __index__)",
    )
    .await?;
    run_async_test(
      10000,
      "op_async_add",
      "assert(await op_async_add(__index__, 100) == __index__ + 100)",
    )
    .await?;
    run_async_test(
      10,
      "op_async_add_smi",
      "assert(await op_async_add_smi(__index__, 100) == __index__ + 100)",
    )
    .await?;
    // See note about overflow on the op method
    run_async_test(
      10,
      "op_async_add_smi",
      "assert(await op_async_add_smi(__index__ * -100, 100) == __index__ * -100 + 100)",
    ).await?;
    Ok(())
  }

  #[op2(async, core)]
  async fn op_async_sleep() {
    tokio::time::sleep(Duration::from_millis(500)).await
  }

  #[op2(async, core)]
  fn op_async_sleep_impl() -> impl Future<Output = ()> {
    tokio::time::sleep(Duration::from_millis(500))
  }

  #[tokio::test]
  pub async fn test_op_async_sleep() -> Result<(), Box<dyn std::error::Error>> {
    run_async_test(5, "op_async_sleep", "await op_async_sleep()").await?;
    run_async_test(5, "op_async_sleep_impl", "await op_async_sleep_impl()")
      .await?;
    Ok(())
  }

  #[op2(async, core)]
  pub async fn op_async_sleep_error() -> Result<(), Error> {
    tokio::time::sleep(Duration::from_millis(500)).await;
    bail!("whoops")
  }

  #[tokio::test]
  pub async fn test_op_async_sleep_error(
  ) -> Result<(), Box<dyn std::error::Error>> {
    run_async_test(
      5,
      "op_async_sleep_error",
      "try { await op_async_sleep_error(); assert(false) } catch (e) {}",
    )
    .await?;
    Ok(())
  }

  #[op2(async(lazy), core)]
  pub async fn op_async_lazy_error() -> Result<(), Error> {
    bail!("whoops")
  }

  #[tokio::test]
  pub async fn test_op_async_lazy_error(
  ) -> Result<(), Box<dyn std::error::Error>> {
    run_async_test(
      5,
      "op_async_lazy_error",
      "try { await op_async_lazy_error(); assert(false) } catch (e) {{ assertErrorContains(e, 'whoops') }}",
    )
    .await?;
    Ok(())
  }

  /// Test exits from the three possible routes -- before future, future immediate,
  /// future polled failed, future polled success.
  #[op2(async, core)]
  pub fn op_async_result_impl(
    mode: u8,
  ) -> Result<impl Future<Output = Result<(), Error>>, Error> {
    if mode == 0 {
      return Err(generic_error("early exit"));
    }
    Ok(async move {
      if mode == 1 {
        return Err(generic_error("early async exit"));
      }
      tokio::time::sleep(Duration::from_millis(500)).await;
      if mode == 2 {
        return Err(generic_error("late async exit"));
      }
      Ok(())
    })
  }

  #[tokio::test]
  pub async fn test_op_async_result_impl(
  ) -> Result<(), Box<dyn std::error::Error>> {
    for (n, msg) in [
      (0, "early exit"),
      (1, "early async exit"),
      (2, "late async exit"),
    ] {
      run_async_test(
        5,
        "op_async_result_impl",
        &format!("try {{ await op_async_result_impl({n}); assert(false) }} catch (e) {{ assertErrorContains(e, '{msg}') }}"),
      )
      .await?;
    }
    run_async_test(5, "op_async_result_impl", "await op_async_result_impl(3);")
      .await?;
    Ok(())
  }

  #[op2(async, core)]
  pub async fn op_async_state_rc(
    state: Rc<RefCell<OpState>>,
    value: u32,
  ) -> u32 {
    let old_value: u32 = state.borrow_mut().take();
    state.borrow_mut().put(value);
    old_value
  }

  #[tokio::test]
  pub async fn test_op_async_state() -> Result<(), Box<dyn std::error::Error>> {
    run_async_test(
      5,
      "op_async_state_rc",
      "if (__index__ == 0) { await op_async_state_rc(__index__) } else { assert(await op_async_state_rc(__index__) == __index__ - 1) }",
    ).await?;
    Ok(())
  }

  #[op2(async, core)]
  #[buffer]
  async fn op_async_buffer(#[buffer] input: JsBuffer) -> JsBuffer {
    input
  }

  #[op2(async, core)]
  #[buffer]
  async fn op_async_buffer_vec(#[buffer] input: JsBuffer) -> Vec<u8> {
    let mut output = input.to_vec();
    output.reverse();
    output
  }

  #[op2(async, core)]
  fn op_async_buffer_impl(#[buffer] input: &[u8]) -> impl Future<Output = u32> {
    let l = input.len();
    async move { l as _ }
  }

  #[tokio::test]
  pub async fn test_op_async_buffer() -> Result<(), Box<dyn std::error::Error>>
  {
    run_async_test(
      2,
      "op_async_buffer",
      "let output = await op_async_buffer(new Uint8Array([1,2,3])); assert(output.length == 3); assert(output[0] == 1);",
    )
    .await?;
    run_async_test(
      2,
      "op_async_buffer_vec",
      "let output = await op_async_buffer_vec(new Uint8Array([3,2,1])); assert(output.length == 3); assert(output[0] == 1);",
    )
    .await?;
    run_async_test(
      2,
      "op_async_buffer_impl",
      "assert(await op_async_buffer_impl(new Uint8Array(10)) == 10)",
    )
    .await?;
    Ok(())
  }

  #[op2(async, core)]
  async fn op_async_external(
    input: *const std::ffi::c_void,
  ) -> *const std::ffi::c_void {
    assert_eq!(input, STRING.as_ptr() as _);
    input
  }

  #[tokio::test]
  pub async fn test_op_async_external() -> Result<(), Box<dyn std::error::Error>>
  {
    run_async_test(
      2,
      "op_external_make, op_async_external",
      "await op_async_external(op_external_make())",
    )
    .await?;
    Ok(())
  }

  #[op2(async, core)]
  #[serde]
  pub async fn op_async_serde_option_v8(
    #[serde] mut serde: Serde,
  ) -> Result<Option<Serde>, AnyError> {
    serde.s += "!";
    Ok(Some(serde))
  }

  #[tokio::test]
  pub async fn test_op_async_serde_option_v8(
  ) -> Result<(), Box<dyn std::error::Error>> {
    run_async_test(
      2,
      "op_async_serde_option_v8",
      "assert((await op_async_serde_option_v8({s: 'abc'})).s == 'abc!')",
    )
    .await?;
    Ok(())
  }
}
