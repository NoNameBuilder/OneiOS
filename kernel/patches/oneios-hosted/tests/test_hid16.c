/*
 * Host test of hosted 0103's rewrite of iOS 27 digitizer events into iOS 16.5's layout (kernel/tests/host.sh builds
 * it): hosted_hid16_element and its helpers, taken from 0103-hid-digitizer-ios16.patch's own lines into hid16.c, on an
 * element made like the Galaxy A56 fork's dumped touch entry (0x178 bytes: a 28-byte header with 2 events, a 0xa0-byte
 * digitizer event at x 0.50002, y 0.43588, finger, identity 2, mask 0x804, radius 5.0, a second header and a child).
 * Checks: the header kept, two 0x68-byte events back to back with x/y as IOFixed (truncated) and the floats from the
 * doubles, the words copied, the rest of the entry zeroed, a second pass leaving it alone, another event type and a
 * child from another sender left alone, a negative and a too-large value saturated or signed.
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "hid16.c"

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static void put32(uint8_t *p, uint32_t v) { memcpy(p, &v, 4); }
static void put64(uint8_t *p, uint64_t v) { memcpy(p, &v, 8); }
static void putd(uint8_t *p, double d) { uint64_t v; memcpy(&v, &d, 8); put64(p, v); }
static uint32_t get32(const uint8_t *p) { uint32_t v; memcpy(&v, p, 4); return v; }
static float getf(const uint8_t *p) { float f; memcpy(&f, p, 4); return f; }

static void
header(uint8_t *p, uint64_t sender, uint32_t count)
{
	put64(p, 0x1122334455667788ull);   /* timestamp */
	put64(p + 8, sender);
	put32(p + 16, 0);                  /* options */
	put32(p + 20, 0);                  /* attribute size */
	put32(p + 24, count);
}

static void
digitizer(uint8_t *ev, double x, double y, uint32_t identity)
{
	put32(ev, 0xa0);
	put32(ev + 4, 11);                 /* kIOHIDEventTypeDigitizer */
	put32(ev + 8, 0x3);                /* options */
	put32(ev + 12, 1);                 /* depth */
	putd(ev + 16, x);
	putd(ev + 24, y);
	putd(ev + 32, 0.0);
	put32(ev + 40, 0);                 /* index */
	put32(ev + 44, 2);                 /* transducer: finger */
	put32(ev + 48, identity);
	put32(ev + 52, 0x804);             /* event mask */
	put32(ev + 56, 0);
	put32(ev + 60, 0);                 /* buttons */
	putd(ev + 64, 0.25);               /* pressure */
	putd(ev + 72, 0.0);
	putd(ev + 80, -1.5);               /* twist */
	put32(ev + 88, 2);                 /* orientation: quality */
	putd(ev + 96, 5.0);                /* major radius */
	putd(ev + 104, 5.0);
	putd(ev + 112, 0.0);
	putd(ev + 120, 0.0);
	putd(ev + 128, 0.0);
	putd(ev + 136, 0.0);
	put32(ev + 144, 7);
	put32(ev + 148, 8);
	put32(ev + 152, 9);
}

int
main(void)
{
	enum { H = 28, IN = 0xa0, OUT = 0x68, LEN = 0x178 };
	uint8_t e[LEN];
	memset(e, 0xee, sizeof(e));
	header(e, 0x100000abcull, 2);
	digitizer(e + H, 0.50002, 0.43588, 2);
	header(e + H + IN, 0x100000abcull, 1);
	digitizer(e + H + IN + H, 0.50002, 0.43588, 2);
	uint8_t hdr[H];
	memcpy(hdr, e, H);

	CHECK(hosted_hid16_element(e, LEN));
	CHECK(memcmp(e, hdr, H) == 0);
	for (unsigned i = 0; i < 2; i++) {
		const uint8_t *o = e + H + i * OUT;
		CHECK(get32(o) == OUT && get32(o + 4) == 11 && get32(o + 8) == 0x3 && get32(o + 12) == 1);
		CHECK(get32(o + 16) == 32769);                     /* 0.50002 * 65536 = 32769.3 */
		CHECK(get32(o + 20) == 28565);                     /* 0.43588 * 65536 = 28565.8 */
		CHECK(get32(o + 24) == 0);
		CHECK(get32(o + 32) == 2 && get32(o + 36) == 2 && get32(o + 40) == 0x804);
		CHECK(getf(o + 52) == 0.25f && getf(o + 56) == 0.0f && getf(o + 60) == -1.5f);
		CHECK(get32(o + 64) == 2 && getf(o + 68) == 5.0f && getf(o + 72) == 5.0f);
		CHECK(get32(o + 92) == 7 && get32(o + 96) == 8 && get32(o + 100) == 9);
	}
	bool zero = true;
	for (unsigned i = H + 2 * OUT; i < LEN; i++) {
		zero = zero && e[i] == 0;
	}
	CHECK(zero);
	uint8_t once[LEN];
	memcpy(once, e, LEN);
	CHECK(!hosted_hid16_element(e, LEN));                  /* already 16.5's layout: left alone */
	CHECK(memcmp(e, once, LEN) == 0);

	/* another event type (a keyboard event, 3), and a child under another sender's header: left alone */
	uint8_t k[LEN];
	header(k, 1, 2);
	digitizer(k + H, 0.5, 0.5, 1);
	header(k + H + IN, 1, 1);
	digitizer(k + H + IN + H, 0.5, 0.5, 1);
	put32(k + H + 4, 3);
	memcpy(once, k, LEN);
	CHECK(!hosted_hid16_element(k, LEN) && memcmp(k, once, LEN) == 0);
	put32(k + H + 4, 11);
	put64(k + H + IN + 8, 2);
	memcpy(once, k, LEN);
	CHECK(!hosted_hid16_element(k, LEN) && memcmp(k, once, LEN) == 0);
	CHECK(!hosted_hid16_element(k, H + IN - 1));          /* too short for one event */

	/* the conversions at their edges */
	uint64_t b;
	double d = -0.5;
	memcpy(&b, &d, 8);
	CHECK((int32_t)hosted_hid16_fixed(b) == -32768);
	d = 1e12;
	memcpy(&b, &d, 8);
	CHECK(hosted_hid16_fixed(b) == 0x7fffffffu);
	d = 1e-30;                                             /* below IOFixed's 1/65536, a float still */
	memcpy(&b, &d, 8);
	CHECK(hosted_hid16_fixed(b) == 0 && hosted_hid16_float(b) != 0);
	d = 1e-50;                                             /* below a float's range: 0 */
	memcpy(&b, &d, 8);
	CHECK(hosted_hid16_float(b) == 0);

	if (failures) {
		printf("test_hid16: %d failures\n", failures);
		return 1;
	}
	printf("test_hid16: ok\n");
	return 0;
}
