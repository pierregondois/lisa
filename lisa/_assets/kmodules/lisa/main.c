/* SPDX-License-Identifier: GPL-2.0 */
#include <linux/module.h>

#include "main.h"
#include "features.h"
#include "introspection.h"
#include "generated/module_version.h"
/* Import all the symbol namespaces that appear to be defined in the kernel
 * sources so that we won't trigger any warning
 */
#include "generated/symbol_namespaces.h"

static char* version = LISA_MODULE_VERSION;
module_param(version, charp, 0);
MODULE_PARM_DESC(version, "Module version defined as sha1sum of the module sources");

static char *features[MAX_FEATURES];
unsigned int features_len = 0;
module_param_array(features, charp, &features_len, 0);
MODULE_PARM_DESC(features, "Comma-separated list of features to enable. Available features are printed when loading the module");

static void modexit(void) {
	if (deinit_features())
		pr_err("Some errors happened while unloading LISA kernel module\n");
}

static int __init modinit(void) {
	int ret;

	pr_info("Loading Lisa module version %s\n", LISA_MODULE_VERSION);
	if (strcmp(version, LISA_MODULE_VERSION)) {
		pr_err("Lisa module version check failed. Got %s, expected %s\n", version, LISA_MODULE_VERSION);
		return -EPROTO;
	}

	pr_info("Kernel features detected. This will impact the module features that are available:\n");
	const char *kernel_feature_names[] = {__KERNEL_FEATURE_NAMES};
	const bool kernel_feature_values[] = {__KERNEL_FEATURE_VALUES};
	for (size_t i=0; i < ARRAY_SIZE(kernel_feature_names); i++) {
		pr_info("  %s: %s\n", kernel_feature_names[i], kernel_feature_values[i] ? "enabled" : "disabled");
	}

	ret = init_features(features_len ? features : NULL , features_len);

	if (ret) {
		pr_err("Some errors happened while loading LISA kernel module\n");

		/* Use one of the standard error code */
		ret = -EINVAL;

		/* If the user selected features manually, make module loading fail so
		 * that they are aware that things went wrong. Otherwise, just
		 * keep going as the user just wanted to enable as many features
		 * as possible.
		 */
		if (features_len) {
			/* Call modexit() explicitly, since it will not be called when ret != 0.
			 * Not calling modexit() can (and will) result in kernel panic handlers
			 * installed by the module are not deregistered before the module code
			 * vanishes.
			 */
			modexit();
			return ret;

		}
	}
	return 0;
}

module_init(modinit);
module_exit(modexit);

MODULE_LICENSE("GPL");

#include <linux/jiffies.h>
#include <linux/slab.h>
#include <linux/fs.h>
#include <linux/types.h>

#include "ftrace_events.h"
#include "wq.h"

struct test_data {
	struct work_item *work;
};

static int free_test_data(struct test_data *data) {
	int ret = 0;
	if (data) {
		ret |= destroy_work(data->work);
		kfree(data);
	}
	return ret;
}

static int test_worker(void *data) {
	int array[] = {1,2,3,4,5,6,7,8,9};
	guid_t guid = GUID_INIT(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11);
	pr_err("tracing event ...\n");
	trace_printk("mytprintk0");
	trace_printk("mytprintk1 %u %llu %s %u %s %x %pS %pB %pUb", 42, 43ull, "a string", 44, "another string", 45, __trace_printk, &__trace_printk, &guid);
	trace_printk("mytprintk2 A%04dB", 52);
	trace_printk("mytprintk3 A%-+12.10dB", 53);
	trace_printk("mytprintk4 A%+010dB", 54);
	trace_printk("mytprintk5 A%#20xB", 55);
	trace_printk("mytprintk6 A%0#20xB", 56);
	trace_printk("mytprintk7 A%+12.10dB", 57);
	trace_printk("mytprintk8 A%+10.12dB", 58);
	trace_printk("mytprintk9  A%-+10.15dB", -59);
	trace_printk("mytprintk10 A%-+*.*dB", 10, 15, -59);
	trace_printk("mytprintk11 A%*.*dB", 10, 15, -59);

	trace_lisa__test_fmt(43, 44, tracepoint_string("hello world"));
	return WORKER_SAME_DELAY;
}

static int enable_event_test(struct feature* feature) {
	int ret = 0;
	struct test_data *data = NULL;
	if (ENABLE_FEATURE(__worqueue))
		return 1;

	data = kzalloc(sizeof(*data), GFP_KERNEL);
	feature->data = data;
	if (!data)
		return 2;

	data->work = start_work(test_worker, msecs_to_jiffies(100), feature);
	if (!data->work)
		ret |= 1;

	return ret;
};

static int disable_event_test(struct feature* feature) {
	int ret = 0;
	struct test_data *data = feature->data;

	if (data)
		free_test_data(data);

	ret |= DISABLE_FEATURE(__worqueue);

	return ret;
};

DEFINE_FEATURE(event__lisa__test_fmt, enable_event_test, disable_event_test);
