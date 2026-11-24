/*
 * mali2metal's channel from user space (the kernel's side: bsd/dev/redwood_m2m.c; the record format
 * and the categories: PLANS/10-mali2metal.md).
 *
 *   m2m_client("metal")              this process is a Metal client: the kernel reports how it ends
 *   m2m_watch_crashes()              ... and where (m2m_crash_record), where the process has no handler of its own
 *   m2m_log('E', "PSO", "...", ...)  one record: severity I/W/E/F, a category of 2-5 capitals
 *
 * (Jesvi-iOS, for its /dev/m2m; on OneiOS the records go where the OneiOS note below says.) The time,
 * the process name and the pid are the kernel's. A record is one line: newlines inside the message
 * become " | ". Where /dev/m2m cannot be opened (an older kernel), records go to stdout as
 * "m2m-nodev: <sev> <CAT> <message>" so that nothing is lost, and the reason is said once.
 */
#ifndef REDWOOD_M2M_H
#define REDWOOD_M2M_H

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/ucontext.h>
#include <unistd.h>

/*
 * OneiOS, the Galaxy A56's Metal plug-in (userland/metal-amd/NOTICE): Jesvi-iOS's m2m channel was a device of its
 * own kernel (/dev/m2m). The A56's guest kernel has none, so the same calls write to two places of OneiOS:
 *
 *   every record    os_log, subsystem "com.oneios.radeonmetal", category CAT, "<sev> <message>" (iOS's own log:
 *                   `oneios-tool phone pull-log DIR`, then `oneios-tool ioslog` on it)
 *   some records    one text row of the kernel (hosted 0039), in xnuhost.log (`oneios-tool phone log`):
 *                       oneios-gpu-text: PROCESS[PID] RM <sev> <CAT> <message, cut at 63 characters>
 *                   for E and F records (at most 12 a process), I and W records of category DEV (the device's
 *                   init: at most 10) and the first two pipelines made (I records of category PSO "pipeline N: ..." or
 *                   "compute pipeline made: ..."). A row is
 *                   streamed as IOServiceOpen calls of types 0x41477fff (a new row) and 0x414770cc (character cc) on
 *                   the accelerator Metal handed the device (m2m_set_service), which the kernel answers with an
 *                   error and turns into one line; a character is one call (about 70 us here), so the row budget
 *                   above is what bounds the cost: never per frame.
 *
 * m2m_client() registered a role with Jesvi's kernel; it is kept as a no-op so the driver's calls stay as they are.
 */
#include <os/log.h>
#include <IOKit/IOKitLib.h>
#include <mach/mach.h>

#define M2M_RECORD_MAX          1024
#define M2M_ROW_MAX             63      /* the kernel's row (0039 hosted_gpu_row.text) */

static io_service_t m2m_service = IO_OBJECT_NULL;   /* the accelerator, once the device has it */
static os_log_t m2m_os_log;
static pthread_once_t m2m_once = PTHREAD_ONCE_INIT;

static void
m2m_open_once(void)
{
	m2m_os_log = os_log_create("com.oneios.radeonmetal", "metal");
}

/* The device hands over the accelerator's service; rows are streamed on it from then on. */
static inline void
m2m_set_service(io_service_t service)
{
	if (service != IO_OBJECT_NULL && m2m_service == IO_OBJECT_NULL) {
		IOObjectRetain(service);
		m2m_service = service;
	}
}

static inline void
m2m_row(const char *text)
{
	static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
	io_connect_t c = IO_OBJECT_NULL;

	if (m2m_service == IO_OBJECT_NULL) {
		return;
	}
	pthread_mutex_lock(&lock);      /* one row at a time: the kernel keeps one pending row a process */
	if (IOServiceOpen(m2m_service, mach_task_self(), 0x41477fff, &c) == KERN_SUCCESS && c != IO_OBJECT_NULL) {
		IOServiceClose(c);      /* (an accelerator that took the marker as a client type: not the A56's) */
	}
	for (unsigned i = 0; text[i] != '\0' && i < M2M_ROW_MAX; i++) {
		unsigned char ch = (unsigned char)text[i];

		c = IO_OBJECT_NULL;
		if (IOServiceOpen(m2m_service, mach_task_self(), 0x41477000u | (ch >= 0x20 && ch < 0x7f ? ch : '?'), &c) ==
		    KERN_SUCCESS && c != IO_OBJECT_NULL) {
			IOServiceClose(c);
		}
	}
	pthread_mutex_unlock(&lock);
}

static inline bool
m2m_wants_row(char sev, const char *cat, const char *rec)
{
	static int errors, dev, pso;

	if (sev == 'E' || sev == 'F') {
		return __atomic_fetch_add(&errors, 1, __ATOMIC_RELAXED) < 12;
	}
	if (!strcmp(cat, "DEV")) {
		return __atomic_fetch_add(&dev, 1, __ATOMIC_RELAXED) < 10;
	}
	if (!strcmp(cat, "PSO") && sev == 'I' &&
	    (!strncmp(rec, "pipeline ", 9) || !strncmp(rec, "compute pipeline made", 21))) {
		return __atomic_fetch_add(&pso, 1, __ATOMIC_RELAXED) < 2;
	}
	return false;
}

static inline void
m2m_vlog(char sev, const char *cat, const char *fmt, va_list ap)
{
	char rec[M2M_RECORD_MAX + 16];
	int n;

	pthread_once(&m2m_once, m2m_open_once);
	n = vsnprintf(rec, sizeof(rec), fmt, ap);
	if (n < 0) {
		return;
	}
	n = n < (int)sizeof(rec) - 1 ? n : (int)sizeof(rec) - 1;
	while (n > 0 && (rec[n - 1] == '\n' || rec[n - 1] == ' ')) {
		n--;
	}
	rec[n] = '\0';
	for (char *nl = strchr(rec, '\n'); nl != NULL; nl = strchr(nl, '\n')) {
		*nl = ' ';      /* one record, one line */
	}
	os_log_with_type(m2m_os_log, sev == 'E' || sev == 'F' ? OS_LOG_TYPE_ERROR : OS_LOG_TYPE_DEFAULT,
	    "%{public}c %{public}s %{public}s", sev, cat, rec);
	if (m2m_service != IO_OBJECT_NULL && m2m_wants_row(sev, cat, rec)) {   /* (records before the device: os_log only) */
		char row[M2M_ROW_MAX + 1];

		snprintf(row, sizeof(row), "RM %c %s %s", sev, cat, rec);
		m2m_row(row);
	}
}

static inline void m2m_log(char sev, const char *cat, const char *fmt, ...) __attribute__((format(printf, 3, 4)));
static inline void
m2m_log(char sev, const char *cat, const char *fmt, ...)
{
	va_list ap;

	va_start(ap, fmt);
	m2m_vlog(sev, cat, fmt, ap);
	va_end(ap);
}

static inline void
m2m_client(const char *role)
{
	(void)role;     /* Jesvi's kernel registration; nothing of the kind here (see the top of this file) */
}

/*
 * Where a client died (OneiOS: to os_log, best effort from a signal handler; Jesvi wrote it to /dev/m2m), this one,
 * written from the signal handler before the process goes, says where: the faulting address, pc, lr and
 * the callers (frame pointers), each as image+offset(symbol) from dladdr, and for code in the shared cache
 * also the address as the cache was built (unslid), which `ipsw dyld a2s` names.
 */
extern const void *_dyld_get_shared_cache_range(size_t *length);
#define M2M_CACHE_BASE          0x180000000UL

static inline size_t
m2m_where(char *out, size_t size, uintptr_t address)
{
	size_t length = 0;
	uintptr_t cache = (uintptr_t)_dyld_get_shared_cache_range(&length);
	Dl_info d;
	int n;

	if (dladdr((const void *)address, &d) != 0 && d.dli_fname != NULL) {
		const char *image = strrchr(d.dli_fname, '/') != NULL ? strrchr(d.dli_fname, '/') + 1 : d.dli_fname;

		n = snprintf(out, size, " %s+%#lx(%s)", image, address - (uintptr_t)d.dli_fbase, d.dli_sname != NULL ? d.dli_sname : "?");
	} else {
		n = snprintf(out, size, " %#lx", address);
	}
	if (n > 0 && (size_t)n < size && cache != 0 && address >= cache && address < cache + length) {
		n += snprintf(out + n, size - (size_t)n, "[cache %#lx]", address - cache + M2M_CACHE_BASE);
	}
	return n > 0 ? ((size_t)n < size ? (size_t)n : size - 1) : 0;
}

static inline void
m2m_crash_record(int signal_number, siginfo_t *info, void *context_in)
{
	ucontext_t *context = context_in;
	char where[M2M_RECORD_MAX - 96] = "";
	size_t used = 0;
	uintptr_t pc = 0, lr = 0, fp = 0;

	if (context != NULL && context->uc_mcontext != NULL) {
		pc = (uintptr_t)context->uc_mcontext->__ss.__pc;
		lr = (uintptr_t)context->uc_mcontext->__ss.__lr;
		fp = (uintptr_t)context->uc_mcontext->__ss.__fp;
	}
	used += (size_t)snprintf(where + used, sizeof(where) - used, " pc");
	used += m2m_where(where + used, sizeof(where) - used, pc);
	used += (size_t)snprintf(where + used, sizeof(where) - used, "; lr");
	used += m2m_where(where + used, sizeof(where) - used, lr);
	used += (size_t)snprintf(where + used, sizeof(where) - used, "; callers");
	/* the frame chain: [fp] the previous frame, [fp + 8] its return address; it only ever goes up the stack */
	for (unsigned i = 0; i < 6 && fp != 0 && (fp & 7) == 0 && used + 8 < sizeof(where); i++) {
		uintptr_t next = ((uintptr_t *)fp)[0], ret = ((uintptr_t *)fp)[1];

		if (ret == 0) {
			break;
		}
		used += m2m_where(where + used, sizeof(where) - used, ret);
		fp = next > fp ? next : 0;
	}
	m2m_log('F', "CRASH", "signal %d at %p:%s", signal_number, info != NULL ? info->si_addr : NULL, where);
}

static void __attribute__((unused))
m2m_crash_handler(int signal_number, siginfo_t *info, void *context)
{
	m2m_crash_record(signal_number, info, context);
	signal(signal_number, SIG_DFL);
	raise(signal_number);
}

/* the handler, for each fatal signal the process has left at its default: a handler of its own is never replaced */
static inline void
m2m_watch_crashes(void)
{
	static const int signals[] = { SIGSEGV, SIGBUS, SIGILL, SIGTRAP, SIGABRT, SIGFPE };
	struct sigaction action, old;

	memset(&action, 0, sizeof(action));
	action.sa_sigaction = m2m_crash_handler;
	action.sa_flags = SA_SIGINFO | SA_RESETHAND;
	for (unsigned i = 0; i < sizeof(signals) / sizeof(signals[0]); i++) {
		if (sigaction(signals[i], NULL, &old) == 0 && !(old.sa_flags & SA_SIGINFO) && old.sa_handler == SIG_DFL) {
			sigaction(signals[i], &action, NULL);
		}
	}
}

#endif /* REDWOOD_M2M_H */
