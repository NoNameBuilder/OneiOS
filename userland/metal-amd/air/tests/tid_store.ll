; Copyright © 2026 OneiOS contributors
; SPDX-License-Identifier: MIT
;
; OneiOS's own compute kernel for the host test (userland/metal-amd/air/host-test.sh): one
; dispatch through air_metal.h, whose result is known without a reference GPU. Written by hand in
; the AIR interface (Jesvi-iOS's docs/dev/air.md section 3, the same metadata as phiconst.ll);
; nothing in it comes out of Apple's libraries. In Metal:
;
;     kernel void tid_store(device uint *out [[buffer(0)]],
;                           uint tid [[thread_index_in_threadgroup]])
;     {
;         out[tid] = tid * 3 + 5;
;     }
;
; air_metal_test dispatches one threadgroup of 4x4 threads over buffers filled with 0xa5, so the
; first six words must come back 5, 8, 11, 14, 17, 20 (0x05 0x08 0x0b 0x0e 0x11 0x14): a dispatch
; that ran nothing leaves a5a5a5a5, one that ignored the index writes 5 everywhere, one that took
; the wrong buffer offset moves the run.

target datalayout = "e-p:64:64:64-i1:8:8-i8:8:8-i16:16:16-i32:32:32-i64:64:64-f32:32:32-f64:64:64-v16:16:16-v24:32:32-v32:32:32-v48:64:64-v64:64:64-v96:128:128-v128:128:128-v192:256:256-v256:256:256-v512:512:512-v1024:1024:1024-n8:16:32"
target triple = "air64-apple-ios16.5.0"

define void @tid_store(ptr addrspace(1) %0, i32 %1) local_unnamed_addr {
  %3 = mul i32 %1, 3
  %4 = add i32 %3, 5
  %5 = zext i32 %1 to i64
  %6 = getelementptr inbounds i32, ptr addrspace(1) %0, i64 %5
  store i32 %4, ptr addrspace(1) %6, align 4
  ret void
}

!air.kernel = !{!14}
!air.version = !{!9}
!air.language_version = !{!10}

!9 = !{i32 2, i32 5, i32 0}
!10 = !{!"Metal", i32 3, i32 0, i32 0}
!14 = !{ptr @tid_store, !15, !16}
!15 = !{}
!16 = !{!17, !18}
!17 = !{i32 0, !"air.buffer", !"air.location_index", i32 0, i32 1, !"air.read_write", !"air.address_space", i32 1, !"air.arg_type_size", i32 4, !"air.arg_type_align_size", i32 4, !"air.arg_type_name", !"uint", !"air.arg_name", !"out"}
!18 = !{i32 1, !"air.thread_index_in_threadgroup", !"air.arg_type_name", !"uint", !"air.arg_name", !"tid"}
