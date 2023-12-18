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


int init_lisa_fs(void);
void exit_lisa_fs(void);

static char *features[MAX_FEATURES];
unsigned int features_len = 0;
module_param_array(features, charp, &features_len, 0);
MODULE_PARM_DESC(features, "Comma-separated list of features to enable. Available features are printed when loading the module");

static void modexit(void) {
	exit_lisa_fs();
	if (deinit_features())
		pr_err("Some errors happened while unloading LISA kernel module\n");
}

/*
 * Note: Cannot initialize global_value list of an unnamed struct in __PARAM
 * using LIST_HEAD_INIT. Need to have a function to do this.
 */
void init_feature_param(void)
{
	struct feature_param *param, **pparam;
	struct feature *feature;

	for_each_feature(feature)
		for_each_feature_param(param, pparam, feature)
			INIT_LIST_HEAD(&param->global_value);
}

static int __init modinit(void) {
	int ret;

	pr_info("Loading Lisa module version %s\n", LISA_MODULE_VERSION);
	if (strcmp(version, LISA_MODULE_VERSION)) {
		pr_err("Lisa module version check failed. Got %s, expected %s\n", version, LISA_MODULE_VERSION);
		return -EPROTO;

	init_feature_param();
	ret = init_lisa_fs();
	if (ret) {
		pr_err("Failed to setup lisa_fs\n");
		return ret;
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

		exit_lisa_fs();

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
