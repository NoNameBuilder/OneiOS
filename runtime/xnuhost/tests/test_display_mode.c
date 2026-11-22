// SPDX-License-Identifier: MIT
// The real display.c against a stand-in helper on the real socket: where the guest's mode comes from.
#define _GNU_SOURCE
#include "xnuhost.h"
#include "xhd.h"
#include <stdatomic.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// display.c ends input.c's touch-to-frame measurement at each shown frame; input.c is not linked here.
void xh_input_frame_shown(void) {}

struct message { char magic[4]; uint32_t a, b, c, d; };
static struct { char path[300]; uint32_t w, h, stride, format; off_t memfd_size; int ok; } helper;

static uint32_t report_w = 1440, report_h = 3120, report_hz = 120;
static const char *fallback_setting;   // panel-fallback= for the next case, NULL: none

static void *helper_main(void *arg)
{
  (void)arg;
  struct sockaddr_un addr = { .sun_family = AF_UNIX };
  strcpy(addr.sun_path, helper.path);
  int fd = socket(AF_UNIX, SOCK_SEQPACKET, 0);
  for (int i = 0; i < 300 && connect(fd, (struct sockaddr *)&addr, sizeof addr); i++) usleep(10000);
  struct message panel = { { 'X', 'H', 'D', 'P' }, report_w, report_h, report_hz, 0 }, hello;
  send(fd, &panel, sizeof panel, MSG_NOSIGNAL);
  char control[CMSG_SPACE(sizeof(int))];
  struct iovec iov = { &hello, sizeof hello };
  struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control };
  if (recvmsg(fd, &msg, 0) == (ssize_t)sizeof hello && !memcmp(hello.magic, "XHD1", 4)) {
    int memfd;
    memcpy(&memfd, CMSG_DATA(CMSG_FIRSTHDR(&msg)), sizeof memfd);
    struct stat st;
    fstat(memfd, &st);
    helper.w = hello.a; helper.h = hello.b; helper.stride = hello.c; helper.format = hello.d; helper.memfd_size = st.st_size;
    helper.ok = 1;
  }
  return NULL;
}

// In a child: display.c's state is once per process. 0 when the frame, the hello and the time are as expected.
static int run(const char *dir, const char *mode, unsigned cw, unsigned ch, unsigned chz, unsigned w, unsigned h, unsigned hz, double most_seconds)
{
  pid_t child = fork();
  if (child) { int status = 1; waitpid(child, &status, 0); return WIFEXITED(status) ? WEXITSTATUS(status) : 99; }
  xh_config c;
  xh_config_defaults(&c);
  strcpy(c.mode, mode);
  strcpy(c.session, dir);
  c.scale = 3; c.width = cw; c.height = ch; c.hz = chz;
  if (fallback_setting && xh_config_set(&c, "panel-fallback", fallback_setting)) _exit(8);
  snprintf(helper.path, sizeof helper.path, "%s/display.sock", dir);
  pthread_t thread;
  pthread_create(&thread, NULL, helper_main, NULL);
  struct timespec t0, t1;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  unsigned gw = 0, gh = 0, ghz = 0;
  if (xh_display_init(&c, &gw, &gh, &ghz)) _exit(2);
  clock_gettime(CLOCK_MONOTONIC, &t1);
  if (gw != w || gh != h || ghz != hz) { fprintf(stderr, "guest %ux%u at %u\n", gw, gh, ghz); _exit(3); }
  if ((t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9 > most_seconds) _exit(4);
  pthread_join(thread, NULL);
  if (!helper.ok || helper.w != w || helper.h != h || helper.stride != w * 4 || helper.format != 1) _exit(5);
  if (helper.memfd_size != (off_t)((size_t)w * 4 * h)) _exit(6);       // what the guest's scanout has to fit: 17.97 MB at 1440x3120
  xh_display *d = xh_display_get();
  if (!d || d->width != w || d->height != h || d->stride != w * 4) _exit(7);
  _exit(0);
}

// Direct frames, the helper being the display helper's own xhd.c: it says it is direct, gets guest RAM and the table,
// copies a frame from scattered guest pages and answers; one that never answers is answered for.
#define RAM_PA 0x800000000ull
#define RAM_BYTES 0x40000u
enum { DW = 16, DH = 40, DSTRIDE = 80 };
static struct { char path[300]; int answer; atomic_int copied, ok; uint64_t seq; } dh;
static uint8_t *ram_map;

static void *direct_helper(void *arg)
{
  (void)arg;
  int fd = xhd_connect(dh.path, 3000, NULL);
  struct xhd_frame frame;
  if (fd < 0 || xhd_send_panel(fd, DW, DH, 120, XHD_PANEL_DIRECT) || xhd_recv_hello(fd, &frame, 3000)) return NULL;
  struct xhd_direct d;
  xhd_direct_init(&d);
  struct xhd_rect damage = { 0, 0, 0, 0 };
  for (int i = 0; i < 300 && !d.pending; i++) { if (xhd_drain_direct(fd, &frame, &damage, &d) < 0) break; usleep(10000); }
  if (d.pending) {
    static uint8_t dst[DW * 4 * DH];
    struct xhd_rect done;
    int ok = xhd_copy_direct(&d, dst, DW * 4, DW, DH, 0, NULL, &done) == 0 && done.w == DW && done.h == DH;
    // the guest's frame: rows of DSTRIDE bytes over pages 3, 1 and 2 of its RAM (4 KiB each), in that order
    for (uint32_t y = 0; y < DH && ok; y++) for (uint32_t x = 0; x < DW * 4; x++) {
      uint64_t at = (uint64_t)y * DSTRIDE + x, page = at / 4096 == 0 ? 3 : at / 4096 == 1 ? 1 : 2;
      if (dst[y * DW * 4 + x] != ram_map[page * 4096 + at % 4096]) ok = 0;
    }
    dh.ok = ok;
    dh.seq = d.seq;
    dh.copied = 1;
    if (dh.answer) xhd_send_done(fd, d.seq, dh.answer == 2 ? XHD_DONE_FAILED : 0);
  }
  usleep(400000);                                     // still connected while xnuhost waits (or gives up)
  xhd_direct_close(&d);
  xhd_frame_close(&frame);
  close(fd);
  return NULL;
}

static atomic_int answered;
static atomic_ullong answered_seq;
static void on_done(void *opaque, uint64_t seq) { (void)opaque; answered_seq = seq; answered++; }

static int run_direct(const char *dir, int answer)
{
  pid_t child = fork();
  if (child) { int status = 1; waitpid(child, &status, 0); return WIFEXITED(status) ? WEXITSTATUS(status) : 99; }
  xh_config c;
  xh_config_defaults(&c);
  strcpy(c.mode, "takeover");
  strcpy(c.session, dir);
  c.scale = 3; c.width = DW; c.height = DH; c.hz = 120;                 // pinned: the report comes later, to the reader
  snprintf(dh.path, sizeof dh.path, "%s/display.sock", dir);
  dh.answer = answer;
  int ram = (int)syscall(SYS_memfd_create, "test-ram", 0u);
  if (ram < 0 || ftruncate(ram, RAM_BYTES)) _exit(2);
  ram_map = mmap(NULL, RAM_BYTES, PROT_READ | PROT_WRITE, MAP_SHARED, ram, 0);
  for (unsigned i = 0; i < RAM_BYTES; i++) ram_map[i] = (uint8_t)(i * 7 + i / 4096);
  unsigned w, h, hz;
  if (xh_display_init(&c, &w, &h, &hz)) _exit(2);
  pthread_t thread;
  pthread_create(&thread, NULL, direct_helper, NULL);
  xh_display_set_ram(ram, RAM_PA, RAM_BYTES);
  for (int i = 0; i < 300 && !xh_display_direct(); i++) usleep(10000);
  if (!xh_display_direct()) _exit(3);
  const xh_gpu_entry segs[] = { { RAM_PA + 0x3000, 0x1000, 0 }, { RAM_PA + 0x1000, 0x2000, 0 } };   // pages 3, then 1 and 2
  const xh_gpu_entry outside[] = { { RAM_PA + RAM_BYTES, 0x1000, 0 } };
  if (xh_display_post(outside, 1, 0, DSTRIDE, DW, DH, 0, 0, DW, DH, 1, on_done, NULL) != -1) _exit(4);   // not RAM: not posted
  if (xh_display_post(segs, 2, 0, DSTRIDE, DW, DH, 0, 0, DW, DH, 2, on_done, NULL)) _exit(5);
  for (int i = 0; i < 300 && !answered; i++) usleep(10000);
  if (!answered || answered_seq != 2 || !dh.copied || !dh.ok || dh.seq != 2) _exit(6);
  if (answer == 2) {                                   // it said it could not copy: xnuhost copies again
    for (int i = 0; i < 100 && xh_display_direct(); i++) usleep(10000);
    if (xh_display_direct()) _exit(7);
  }
  pthread_join(thread, NULL);
  _exit(0);
}

int main(void)
{
  char dir[] = "/tmp/xnuhost-display-XXXXXX";
  if (!mkdtemp(dir)) return 2;
  xh_log_open(NULL, XH_LOG_ERROR);
  int failures = 0, r;
#define CASE(name, ...) do { r = run(dir, __VA_ARGS__); printf("%s: %s (%d)\n", name, r ? "FAILED" : "ok", r); failures += r != 0; } while (0)
  CASE("a session takes the helper's report", "takeover", 0, 0, 0, 1440, 3120, 120, 5.0);
  report_w = 1260; report_h = 2800; report_hz = 90;                      // not the default: only the report can give this
  CASE("another panel's report is the guest's mode", "takeover", 0, 0, 0, 1260, 2800, 90, 5.0);
  report_w = 1080; report_h = 2340; report_hz = 120;                    // the base S25's PA1 panel
  CASE("the S25's panel report is the guest's mode", "takeover", 0, 0, 0, 1080, 2340, 120, 5.0);
  report_w = 1440; report_h = 3120; report_hz = 120;
  CASE("a configured mode wins, without waiting", "takeover", 1080, 2340, 60, 1080, 2340, 60, 1.0);
  CASE("a bench run assumes the S24 Ultra's, without waiting", "bench", 0, 0, 0, 1440, 3120, 120, 1.0);
  fallback_setting = "1080x2340@120";                                     // the Galaxy S25's and A56's panel
  CASE("panel-fallback= is what a bench run assumes, without waiting", "bench", 0, 0, 0, 1080, 2340, 120, 1.0);
  CASE("a helper's report still wins over panel-fallback=", "takeover", 0, 0, 0, 1440, 3120, 120, 5.0);
  fallback_setting = NULL;
  r = run_direct(dir, 1);
  printf("direct frames: the helper copies from guest RAM and answers: %s (%d)\n", r ? "FAILED" : "ok", r); failures += r != 0;
  r = run_direct(dir, 2);
  printf("direct frames: a helper that could not copy one turns them off: %s (%d)\n", r ? "FAILED" : "ok", r); failures += r != 0;
  r = run_direct(dir, 0);
  printf("direct frames: a helper that never answers is answered for: %s (%d)\n", r ? "FAILED" : "ok", r); failures += r != 0;
  char command[100];
  snprintf(command, sizeof command, "rm -rf %s", dir);
  if (system(command)) failures++;
  printf("display mode tests: %d failed\n", failures);
  return failures != 0;
}
