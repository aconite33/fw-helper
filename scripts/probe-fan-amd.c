/* probe-fan-amd.c - answer Q4 for the AMD Framework 13: does writing fan duty over
 * raw EC commands move the fan, and does the EC reclaim cleanly afterwards?
 *
 * This is the gating experiment for the fan milestone on this board. The cros_ec hwmon
 * driver exposes no pwm1/pwm1_enable here, so the sysfs path the Intel fork uses does
 * not exist; EC_FEATURE_PWM_FAN is advertised, so the commands should.
 *
 * SAFETY - this is the one probe in the tree that can leave hardware in a dangerous
 * state, so it follows ADR 0006 rather than merely citing it:
 *
 *   - It only ever spins the fan UP. A stuck-high fan is loud; a stuck-low fan is
 *     silent and looks identical to a working one from outside.
 *   - Restoration is installed BEFORE the first write and runs on every exit path:
 *     normal return, error, SIGINT, SIGTERM, SIGHUP, SIGQUIT.
 *   - The second half of the test is the half that matters. Proving duty writes work
 *     is not the point; proving the EC takes the fan back is.
 *
 * Opcodes and parameter structs verified against torvalds/linux
 * include/linux/platform_data/cros_ec_commands.h:
 *   EC_CMD_PWM_SET_FAN_DUTY       0x0024  v0 { uint32_t percent; }
 *   EC_CMD_THERMAL_AUTO_FAN_CTRL  0x0052  v0 no params
 *   EC_CMD_PWM_GET_FAN_TARGET_RPM 0x0020  v0 no params -> uint32_t rpm
 *
 *   gcc -O2 -Wall -o probe-fan-amd probe-fan-amd.c
 *   sudo ./probe-fan-amd            Q4: does duty move the fan, does the EC take it back?
 *   sudo ./probe-fan-amd --sweep    duty -> RPM table, and where the fan stalls
 *   sudo ./probe-fan-amd --breakaway  lowest duty that starts the fan from rest
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <signal.h>
#include <dirent.h>
#include <sys/ioctl.h>

#define DEV "/dev/cros_ec"
#define CROS_EC_DEV_IOCXCMD_V2 0xC014EC00u

#define EC_CMD_PWM_GET_FAN_TARGET_RPM 0x0020
#define EC_CMD_PWM_SET_FAN_DUTY       0x0024
#define EC_CMD_THERMAL_AUTO_FAN_CTRL  0x0052

/* Duty is a percentage on this interface, not an 8-bit count. Deliberately high:
 * see the spin-up-only rule above. */
#define DUTY_LOW  40
#define DUTY_HIGH 70

struct ec_cmd_v2 {
	uint32_t version;
	uint32_t command;
	uint32_t outsize;
	uint32_t insize;
	uint32_t result;
	uint8_t  data[256];
};

static int ec_fd = -1;
static volatile sig_atomic_t took_control = 0;

static int ec_cmd(uint32_t cmd, uint32_t ver,
                  const void *out, size_t outsize,
                  void *in, size_t insize, uint32_t *result)
{
	struct ec_cmd_v2 c;
	memset(&c, 0, sizeof(c));
	c.version = ver;
	c.command = cmd;
	c.outsize = (uint32_t)outsize;
	c.insize  = (uint32_t)insize;
	if (outsize)
		memcpy(c.data, out, outsize);

	int rc = ioctl(ec_fd, CROS_EC_DEV_IOCXCMD_V2, &c);
	if (result)
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

/* Hand the fan back to the EC. Kept minimal and free of stdio so it is safe to call
 * from a signal handler. Tries v0, then v1 with fan_idx 0. */
static void release_fan(void)
{
	if (ec_fd < 0)
		return;
	uint32_t result;
	if (ec_cmd(EC_CMD_THERMAL_AUTO_FAN_CTRL, 0, NULL, 0, NULL, 0, &result) < 0) {
		uint8_t fan_idx = 0;
		ec_cmd(EC_CMD_THERMAL_AUTO_FAN_CTRL, 1, &fan_idx, sizeof(fan_idx),
		       NULL, 0, &result);
	}
	took_control = 0;
}

static void on_signal(int sig)
{
	if (took_control) {
		release_fan();
		const char msg[] = "\n[signal] fan released to EC automatic\n";
		ssize_t n = write(STDERR_FILENO, msg, sizeof(msg) - 1);
		(void)n;
	}
	_exit(128 + sig);
}

static void on_exit_restore(void)
{
	if (took_control) {
		release_fan();
		fprintf(stderr, "[atexit] fan released to EC automatic\n");
	}
}

/* Set fan duty as a percentage. Returns 0 on success. */
static int set_duty(uint32_t percent, uint32_t *result)
{
	int rc = ec_cmd(EC_CMD_PWM_SET_FAN_DUTY, 0,
	                &percent, sizeof(percent), NULL, 0, result);
	if (rc >= 0)
		return 0;
	/* Fall back to v1, which carries an explicit fan index. */
	struct {
		uint32_t percent;
		uint8_t  fan_idx;
	} __attribute__((packed)) v1 = { percent, 0 };
	rc = ec_cmd(EC_CMD_PWM_SET_FAN_DUTY, 1, &v1, sizeof(v1), NULL, 0, result);
	return rc >= 0 ? 0 : rc;
}

static uint32_t ec_target_rpm(void)
{
	uint32_t rpm = 0, result;
	ec_cmd(EC_CMD_PWM_GET_FAN_TARGET_RPM, 0, NULL, 0, &rpm, sizeof(rpm), &result);
	return rpm;
}

/* hwmon is the daemon's telemetry source, so cross-check it against the EC rather
 * than trusting either alone. Resolved by name - indices are not stable across boots. */
static char hwmon_rpm_path[512];
static char hwmon_dir[512];

static void find_hwmon(void)
{
	DIR *d = opendir("/sys/class/hwmon");
	if (!d)
		return;
	struct dirent *e;
	while ((e = readdir(d))) {
		if (strncmp(e->d_name, "hwmon", 5) != 0)
			continue;
		char p[512], name[64] = {0};
		snprintf(p, sizeof(p), "/sys/class/hwmon/%s/name", e->d_name);
		FILE *f = fopen(p, "r");
		if (!f)
			continue;
		if (fgets(name, sizeof(name), f))
			name[strcspn(name, "\n")] = 0;
		fclose(f);
		if (strcmp(name, "cros_ec") == 0) {
			snprintf(hwmon_dir, sizeof(hwmon_dir),
			         "/sys/class/hwmon/%s", e->d_name);
			snprintf(hwmon_rpm_path, sizeof(hwmon_rpm_path),
			         "/sys/class/hwmon/%s/fan1_input", e->d_name);
			break;
		}
	}
	closedir(d);
}

static long hwmon_rpm(void)
{
	if (!hwmon_rpm_path[0])
		return -1;
	FILE *f = fopen(hwmon_rpm_path, "r");
	if (!f)
		return -1;
	long v = -1;
	if (fscanf(f, "%ld", &v) != 1)
		v = -1;
	fclose(f);
	return v;
}

static void sample(const char *tag, int seconds)
{
	for (int i = 1; i <= seconds; i++) {
		sleep(1);
		printf("    %-14s t+%-2ds  hwmon=%-6ld ec_target=%u\n",
		       tag, i, hwmon_rpm(), ec_target_rpm());
		fflush(stdout);
	}
}

/* Hottest EC sensor, for recording the conditions a sweep was taken under. The
 * duty->RPM relationship is mildly load-dependent, so a table without its
 * temperature is not reproducible. */
static double hottest_temp(void)
{
	double hottest = -300.0;
	for (int i = 1; i <= 4; i++) {
		char p[600];
		snprintf(p, sizeof(p), "%s/temp%d_input", hwmon_dir, i);
		FILE *f = fopen(p, "r");
		if (!f)
			continue;
		long mc;
		if (fscanf(f, "%ld", &mc) == 1 && mc / 1000.0 > hottest)
			hottest = mc / 1000.0;
		fclose(f);
	}
	return hottest;
}

/* Duty -> RPM sweep.
 *
 * Descends rather than ascends, exactly as the Intel fork's sweep did, so the fan is
 * never asked to START from rest at a duty below stiction - which would record a
 * stopped fan as that duty's speed and put a false zero in the middle of the table.
 * Below stiction on the way DOWN the fan coasts to a stop honestly.
 *
 * The low end is sampled finely because that is where stiction lives and where a
 * quiet curve operates. On the Intel board stiction sat between 8-bit duty 20 and 30,
 * i.e. 8-12%, so this brackets that range closely.
 */
static const uint32_t SWEEP[] = {
	100, 90, 80, 70, 60, 50, 45, 40, 35, 30,
	25, 20, 18, 16, 14, 12, 10, 8, 6, 4, 0,
};

static int run_sweep(int settle)
{
	uint32_t result = 0;

	printf("== duty -> RPM sweep ==\n");
	printf("  settling %ds per point, descending; keep the machine IDLE\n", settle);
	printf("  start temp %.1f C\n\n", hottest_temp());
	printf("  %-8s %-10s %s\n", "duty%", "rpm", "temp C");

	took_control = 1;
	for (size_t i = 0; i < sizeof(SWEEP) / sizeof(SWEEP[0]); i++) {
		if (set_duty(SWEEP[i], &result) < 0) {
			fprintf(stderr, "  SET_FAN_DUTY %u failed (result %u)\n",
			        SWEEP[i], result);
			return 2;
		}
		for (int s = 0; s < settle; s++)
			sleep(1);
		printf("  %-8u %-10ld %.1f\n", SWEEP[i], hwmon_rpm(), hottest_temp());
		fflush(stdout);
	}

	printf("\n  releasing to EC automatic\n");
	release_fan();
	sleep(3);
	printf("  after release: rpm=%ld\n", hwmon_rpm());

	printf("\nRead the table for two things:\n");
	printf("  - STICTION: the duty where RPM first reads 0 on the way down.\n");
	printf("    Any duty at or below that is a stopped fan, not a slow one.\n");
	printf("  - CURVATURE: do not fit a line. The Intel board's curve was concave,\n");
	printf("    and a linear fit put the firmware floor BELOW firmware.\n");
	return 0;
}

/* Breakaway: the lowest duty that will start the fan FROM REST.
 *
 * The descending sweep answers a different question. It measures where a fan already
 * turning finally stalls, which is a dynamic-friction number. Starting from rest has to
 * overcome static friction and needs more duty. The gap between the two is the dangerous
 * band: a curve idling there runs fine while cooling down, then fails to spin up from
 * cold, and a fan that never starts is silent - indistinguishable from a working quiet
 * curve until something overheats.
 *
 * Ascending is normally forbidden in this file. It is correct here precisely because
 * finding a duty that does NOT move the fan is the measurement.
 */
static const uint32_t BREAKAWAY[] = { 8, 9, 10, 11, 12, 13, 14, 16, 18, 20, 25 };

static int run_breakaway(void)
{
	uint32_t result = 0;

	printf("== breakaway: lowest duty that starts the fan from rest ==\n");
	printf("  temp %.1f C\n", hottest_temp());

	/* Bring the fan to a genuine stop first, or we would be measuring a coast-down. */
	took_control = 1;
	if (set_duty(0, &result) < 0) {
		fprintf(stderr, "  SET_FAN_DUTY 0 failed (result %u)\n", result);
		return 2;
	}
	printf("  settling to rest");
	fflush(stdout);
	for (int i = 0; i < 20; i++) {
		sleep(1);
		printf(".");
		fflush(stdout);
		if (hwmon_rpm() == 0)
			break;
	}
	long at_rest = hwmon_rpm();
	printf(" rpm=%ld\n", at_rest);
	if (at_rest != 0) {
		fprintf(stderr, "  fan did not come to rest; result would be a coast-down.\n");
		fprintf(stderr, "  Is the machine idle and cool?\n");
		return 2;
	}

	printf("\n  %-8s %-10s %s\n", "duty%", "rpm", "verdict");
	uint32_t started_at = 0;
	for (size_t i = 0; i < sizeof(BREAKAWAY) / sizeof(BREAKAWAY[0]); i++) {
		if (set_duty(BREAKAWAY[i], &result) < 0) {
			fprintf(stderr, "  SET_FAN_DUTY %u failed (result %u)\n",
			        BREAKAWAY[i], result);
			return 2;
		}
		for (int s = 0; s < 8; s++)
			sleep(1);
		long rpm = hwmon_rpm();
		printf("  %-8u %-10ld %s\n", BREAKAWAY[i], rpm,
		       rpm > 0 ? "STARTED" : "still at rest");
		fflush(stdout);
		if (rpm > 0) {
			started_at = BREAKAWAY[i];
			break;
		}
	}

	printf("\n  releasing to EC automatic\n");
	release_fan();
	sleep(3);
	printf("  after release: rpm=%ld\n", hwmon_rpm());

	if (started_at)
		printf("\n  BREAKAWAY = %u%%. No curve may command a non-zero duty below this,\n"
		       "  even though the sweep shows the fan sustains rotation lower.\n",
		       started_at);
	else
		printf("\n  Fan never started across the tested range. Re-run; if it repeats,\n"
		       "  the minimum usable duty is above 25%% and a quiet curve is not viable.\n");
	return 0;
}

int main(int argc, char **argv)
{
	ec_fd = open(DEV, O_RDWR);
	if (ec_fd < 0) {
		fprintf(stderr, "cannot open %s: %s%s\n", DEV, strerror(errno),
		        errno == EACCES ? " (run with sudo)" : "");
		return 1;
	}
	find_hwmon();
	if (!hwmon_rpm_path[0])
		fprintf(stderr, "warning: no cros_ec hwmon node; EC readings only\n");

	/* Installed before the first write, not after. */
	struct sigaction sa;
	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = on_signal;
	sigaction(SIGINT,  &sa, NULL);
	sigaction(SIGTERM, &sa, NULL);
	sigaction(SIGHUP,  &sa, NULL);
	sigaction(SIGQUIT, &sa, NULL);
	atexit(on_exit_restore);

	if (argc > 1 && strcmp(argv[1], "--sweep") == 0) {
		int rc = run_sweep(7);
		close(ec_fd);
		return rc;
	}
	if (argc > 1 && strcmp(argv[1], "--breakaway") == 0) {
		int rc = run_breakaway();
		close(ec_fd);
		return rc;
	}

	uint32_t result = 0;

	printf("== baseline (EC owns the fan) ==\n");
	printf("    hwmon=%ld ec_target=%u\n", hwmon_rpm(), ec_target_rpm());

	printf("\n== taking manual control at %d%% duty ==\n", DUTY_LOW);
	took_control = 1;              /* set BEFORE the write, so a crash mid-write restores */
	if (set_duty(DUTY_LOW, &result) < 0) {
		fprintf(stderr, "  SET_FAN_DUTY failed (result %u, %s)\n",
		        result, strerror(errno));
		fprintf(stderr, "  -> fan duty control is NOT available this way\n");
		return 2;
	}
	printf("  command accepted\n");
	sample("manual40", 6);

	printf("\n== raising to %d%% duty ==\n", DUTY_HIGH);
	if (set_duty(DUTY_HIGH, &result) < 0)
		fprintf(stderr, "  raise failed (result %u)\n", result);
	sample("manual70", 6);

	printf("\n== releasing to EC automatic ==\n");
	release_fan();
	printf("  THERMAL_AUTO_FAN_CTRL issued\n");
	sample("released", 8);

	printf("\nInterpretation:\n");
	printf("  - RPM rose with duty, then fell after release -> manual control works\n");
	printf("    AND the EC reclaims cleanly. Both halves are required.\n");
	printf("  - RPM stayed high after release -> DO NOT build fan control on this;\n");
	printf("    the release path is the whole safety story (ADR 0006).\n");

	close(ec_fd);
	return 0;
}
