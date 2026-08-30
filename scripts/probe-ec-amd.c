/* probe-ec-amd.c - read-only EC interrogation for the AMD Framework 13 fork.
 *
 * Answers three questions that gate the fork, without writing anything:
 *
 *   1. Does this EC advertise EC_FEATURE_PWM_FAN?  The cros_ec hwmon driver exposes
 *      no pwm1/pwm1_enable on this board, so fan control must go through raw EC
 *      commands or not at all.
 *   2. Does Framework's custom charge-limit command (0x3E03, ADR 0012) answer here?
 *      That is the one mechanism expected to port unchanged from the Intel board.
 *   3. What does the EC report for fan RPM through its own command, as a cross-check
 *      against hwmon's read-only fan1_input?
 *
 * Every command issued is a read. Nothing here changes hardware state.
 *
 * Opcodes verified against torvalds/linux
 * include/linux/platform_data/cros_ec_commands.h, not from memory - an opcode from
 * memory is a coin flip, and the EC answers a different question rather than erroring.
 *
 *   gcc -O2 -Wall -o probe-ec-amd probe-ec-amd.c && sudo ./probe-ec-amd
 */

#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/ioctl.h>

#define DEV "/dev/cros_ec"

/* _IOWR(0xEC, 0, struct cros_ec_command_v2) - 20-byte header, matches the value
 * pinned in crates/fw-helperd/src/ec.rs (0xC014EC00). */
#define CROS_EC_DEV_IOCXCMD_V2 0xC014EC00u

#define EC_CMD_GET_FEATURES           0x000D
#define EC_CMD_PWM_GET_FAN_TARGET_RPM 0x0020
#define EC_CMD_CHARGE_LIMIT_CONTROL   0x3E03

#define EC_FEATURE_PWM_FAN 2
#define EC_FEATURE_THERMAL 10

#define CHARGE_LIMIT_GET 0x08

struct ec_cmd_v2 {
	uint32_t version;
	uint32_t command;
	uint32_t outsize;
	uint32_t insize;
	uint32_t result;
	uint8_t  data[256];
};

/* Returns bytes read on success, -1 on transport failure, -2 when the EC itself
 * declined the command (result != 0) - the distinction that matters, because a
 * decline means firmware without that command rather than a broken path. */
static int ec_cmd(int fd, uint32_t cmd, uint32_t ver,
                  const uint8_t *out, size_t outsize,
                  uint8_t *in, size_t insize, uint32_t *result)
{
	struct ec_cmd_v2 c;
	memset(&c, 0, sizeof(c));
	c.version = ver;
	c.command = cmd;
	c.outsize = (uint32_t)outsize;
	c.insize  = (uint32_t)insize;
	if (outsize)
		memcpy(c.data, out, outsize);

	int rc = ioctl(fd, CROS_EC_DEV_IOCXCMD_V2, &c);
	*result = c.result;
	if (rc < 0)
		return -1;
	if (c.result != 0)
		return -2;
	if (insize && in) {
		size_t got = (size_t)rc < insize ? (size_t)rc : insize;
		memcpy(in, c.data, got);
	}
	return rc;
}

static void report(const char *what, int rc, uint32_t result)
{
	if (rc == -1)
		printf("  %-34s TRANSPORT FAIL (%s)\n", what, strerror(errno));
	else if (rc == -2)
		printf("  %-34s EC DECLINED (result %u)\n", what, result);
}

int main(void)
{
	int fd = open(DEV, O_RDWR);
	if (fd < 0) {
		fprintf(stderr, "cannot open %s: %s%s\n", DEV, strerror(errno),
		        errno == EACCES ? " (run with sudo)" : "");
		return 1;
	}

	uint32_t result;
	int rc;

	printf("== EC features (0x%04X) ==\n", EC_CMD_GET_FEATURES);
	uint32_t flags[2] = {0, 0};
	rc = ec_cmd(fd, EC_CMD_GET_FEATURES, 0, NULL, 0,
	            (uint8_t *)flags, sizeof(flags), &result);
	report("GET_FEATURES", rc, result);
	if (rc >= 0) {
		printf("  flags[0]=0x%08X flags[1]=0x%08X\n", flags[0], flags[1]);
		int pwm_fan = (flags[0] >> EC_FEATURE_PWM_FAN) & 1;
		int thermal = (flags[0] >> EC_FEATURE_THERMAL) & 1;
		printf("  %-34s %s\n", "EC_FEATURE_PWM_FAN (bit 2)",
		       pwm_fan ? "YES - fan duty control exists" : "NO");
		printf("  %-34s %s\n", "EC_FEATURE_THERMAL (bit 10)",
		       thermal ? "YES" : "NO");
	}

	printf("\n== Fan RPM via EC (0x%04X) ==\n", EC_CMD_PWM_GET_FAN_TARGET_RPM);
	uint32_t rpm = 0;
	rc = ec_cmd(fd, EC_CMD_PWM_GET_FAN_TARGET_RPM, 0, NULL, 0,
	            (uint8_t *)&rpm, sizeof(rpm), &result);
	report("PWM_GET_FAN_TARGET_RPM", rc, result);
	if (rc >= 0)
		printf("  target rpm = %u\n", rpm);

	printf("\n== Framework charge limit (0x%04X, ADR 0012) ==\n",
	       EC_CMD_CHARGE_LIMIT_CONTROL);
	uint8_t req[3] = { CHARGE_LIMIT_GET, 0xFF, 0xFF };
	uint8_t resp[2] = { 0, 0 };
	rc = ec_cmd(fd, EC_CMD_CHARGE_LIMIT_CONTROL, 0,
	            req, sizeof(req), resp, sizeof(resp), &result);
	report("CHARGE_LIMIT_CONTROL get", rc, result);
	if (rc >= 0)
		printf("  max=%u%% min=%u%%   <- the limit that governs charging\n",
		       resp[0], resp[1]);

	close(fd);
	return 0;
}
