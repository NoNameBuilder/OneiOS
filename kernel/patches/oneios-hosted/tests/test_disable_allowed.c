/*
 * Host test of hosted 0098's rename (kernel/tests/host.sh builds it): oneios_disable_allowed_marks_in, taken from
 * 0098-disable-allowed.patch's own lines into marks.c, on made-up binary plists (no Apple file in the repository).
 * Checks: every key string object "DisableAllowed" (0x5e + 14 bytes) is counted and renamed in place, nothing else of
 * the buffer changes (same length, no offset moves), a longer string that only starts with the name (0x5f marker) and
 * the bare bytes without the marker are left alone, the count of the renamed name afterwards, and a buffer that is no
 * binary plist is refused (-1).
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "marks.c"

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

int
main(void)
{
	uint8_t plist[256];
	size_t n = 0;
	memcpy(plist, "bplist00", 8);
	n = 8;
	/* a dictionary's keys: "JetsamPriority" (0x5e, also 14 bytes), "DisableAllowed" twice, then the value false (0x08) */
	const char *keys[] = {"JetsamPriority", "DisableAllowed", "DisableAllowed"};
	for (unsigned k = 0; k < 3; k++) {
		plist[n++] = 0x5e;
		memcpy(plist + n, keys[k], 14);
		n += 14;
		plist[n++] = 0x08;
	}
	/* a 15-byte string that starts with the name: marker 0x5f, must stay */
	plist[n++] = 0x5f;
	memcpy(plist + n, "DisableAllowedX", 15);
	n += 15;
	/* the bare bytes with another marker in front (0x0f is a fill byte): must stay */
	plist[n++] = 0x0f;
	memcpy(plist + n, "DisableAllowed", 14);
	n += 14;
	/* a key at the very end of the buffer */
	plist[n++] = 0x5e;
	memcpy(plist + n, "DisableAllowed", 14);
	n += 14;

	uint8_t before[256];
	memcpy(before, plist, n);
	CHECK(oneios_disable_allowed_marks_in(plist, (uint32_t)n, "DisableAllowed", NULL) == 3);
	CHECK(memcmp(before, plist, n) == 0);             /* counting changes nothing */
	CHECK(oneios_disable_allowed_marks_in(plist, (uint32_t)n, "DisableAllowed", "OneiOSDisAllow") == 3);
	CHECK(oneios_disable_allowed_marks_in(plist, (uint32_t)n, "DisableAllowed", NULL) == 0);
	CHECK(oneios_disable_allowed_marks_in(plist, (uint32_t)n, "OneiOSDisAllow", NULL) == 3);
	unsigned changed = 0;
	for (size_t i = 0; i < n; i++) {
		changed += before[i] != plist[i];
	}
	/* "DisableAllowed" -> "OneiOSDisAllow": the bytes that differ, 3 times */
	unsigned per = 0;
	for (unsigned i = 0; i < 14; i++) {
		per += "DisableAllowed"[i] != "OneiOSDisAllow"[i];
	}
	CHECK(changed == 3 * per);
	CHECK(memcmp(plist + 8 + 1, "JetsamPriority", 14) == 0);
	CHECK(memcmp(plist + n - 14 - 1 - 14 - 1 - 15, "DisableAllowedX", 15) == 0);
	CHECK(memcmp(plist + n - 14 - 1 - 14, "DisableAllowed", 14) == 0);   /* the bare bytes */
	CHECK(memcmp(plist + n - 14, "OneiOSDisAllow", 14) == 0);           /* the last key */

	uint8_t xml[] = "<?xml version=\"1.0\"?><plist><dict><key>DisableAllowed</key><false/></dict></plist>";
	CHECK(oneios_disable_allowed_marks_in(xml, (uint32_t)sizeof(xml), "DisableAllowed", "OneiOSDisAllow") == -1);
	CHECK(oneios_disable_allowed_marks_in(plist, 4, "DisableAllowed", NULL) == -1);

	if (failures) {
		return 1;
	}
	printf("PASS: 0098's rename: 3 marks counted and renamed in place, a longer string, the bare bytes and a key at the end, "
	    "not a binary plist refused\n");
	return 0;
}
