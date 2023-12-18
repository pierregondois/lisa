/* SPDX-License-Identifier: GPL-2.0 */

#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/seq_file.h>
#include <linux/slab.h>
#include <linux/user_namespace.h>
#include <linux/percpu-rwsem.h>

#include "features.h"
#include "configs.h"

static int lisa_fs_create_files(struct dentry *dentry, struct lisa_cfg *cfg);
static struct dentry *
lisa_fs_create_single(struct dentry *parent, const char *name,
		      const struct inode_operations *i_ops,
		      const struct file_operations *f_ops, umode_t mode,
		      void *data);

#define LISA_FS_SUPER_MAGIC 0xcdb11bc9

struct lisa_sb_info {
	/* Protect the interface. */
	struct mutex interface_lock;

	/* List of configs. */
	struct hlist_head cfg_list;
};

static inline void lisa_sb_lock(struct super_block *sb)
{
	struct lisa_sb_info *si = sb->s_fs_info;
	mutex_lock(&si->interface_lock);
}

static inline void lisa_sb_unlock(struct super_block *sb)
{
	struct lisa_sb_info *si = sb->s_fs_info;
	mutex_unlock(&si->interface_lock);
}

static inline struct hlist_head *lisa_sb_get_cfg_list(struct super_block *sb)
{
	struct lisa_sb_info *si = sb->s_fs_info;

	/* If vfs initialization failed. */
	if (!si)
		return NULL;
	return &si->cfg_list;
}

static struct inode *lisa_fs_create_inode(struct super_block *sb, int mode)
{
	struct inode *inode = new_inode(sb);

	if (inode) {
		inode->i_ino = get_next_ino();
		inode->i_mode = mode;
		inode->i_atime = inode->i_mtime = current_time(inode);
	}

	return inode;
}

/*
 * available_features handlers
 */

static int lisa_features_available_show(struct seq_file *s, void *data)
{
	struct feature *feature;

	for_each_feature(feature)
		if (!feature->__internal)
			seq_printf(s, "%s\n", feature->name);

	return 0;
}

static int lisa_features_available_open(struct inode *inode, struct file *file)
{
	return single_open(file, lisa_features_available_show, NULL);
}

static struct file_operations lisa_available_features_fops = {
	.owner = THIS_MODULE,
	.open = lisa_features_available_open,
	.read = seq_read,
	.llseek = seq_lseek,
	.release = single_release,
};

/*
 * activate handlers
 */

static int lisa_activate_show(struct seq_file *s, void *data)
{
	struct lisa_cfg *cfg = s->private;

	seq_printf(s, "%d\n", cfg->activated);
	return 0;
}

static ssize_t lisa_activate_write(struct file *file, const char __user *buf,
				   size_t count, loff_t *ppos)
{
	struct seq_file *s = file->private_data;
	bool value;
	int ret;

	if (kstrtobool_from_user(buf, count, &value))
		return -EINVAL;

	lisa_sb_lock(file->f_inode->i_sb);
	ret = activate_lisa_cfg((struct lisa_cfg *)s->private, value);
	lisa_sb_unlock(file->f_inode->i_sb);

	return ret < 0 ? ret : count;
}

static int lisa_activate_open(struct inode *inode, struct file *file)
{
	struct lisa_cfg *cfg = inode->i_private;

	return single_open(file, lisa_activate_show, cfg);
}

static struct file_operations lisa_activate_fops = {
	.owner = THIS_MODULE,
	.open = lisa_activate_open,
	.read = seq_read,
	.write = lisa_activate_write,
	.release = single_release,
};

/*
 * set_features handlers
 * available_features handlers
 */

static void *lisa_param_feature_seq_start(struct seq_file *s, loff_t *pos)
{
	struct feature_param_entry *entry;
	void *ret;

	lisa_sb_lock(s->file->f_inode->i_sb);

	entry = *(struct feature_param_entry **)s->private;
	ret = seq_list_start(&entry->list_values, *pos);

	return ret;
}

static int lisa_param_feature_seq_show(struct seq_file *s, void *v)
{
	struct feature_param_entry_value *val;
	struct feature_param_entry *entry;
	struct feature_param *param;

	entry = *(struct feature_param_entry **)s->private;
	param = entry->param;

	val = hlist_entry(v, struct feature_param_entry_value, node);

	if (param->ops->stringify) {
		size_t size = param->ops->stringify(val, NULL);
		char *buf = kmalloc(size + 1, GFP_KERNEL);

		if (!buf)
			return -ENOMEM;

		buf[size] = '\0';
		size = param->ops->stringify(val, buf);
		seq_printf(s, "%s\n", buf);
		kfree(buf);
	}

	return 0;
}

static void *lisa_param_feature_seq_next(struct seq_file *s, void *v, loff_t *pos)
{
	struct feature_param_entry *entry;
	entry = *(struct feature_param_entry **)s->private;

	return seq_list_next(v, &entry->list_values, pos);
}

static void lisa_param_feature_seq_stop(struct seq_file *s, void *v)
{
	lisa_sb_unlock(s->file->f_inode->i_sb);
}

static const struct seq_operations lisa_param_feature_seq_ops = {
	.start = lisa_param_feature_seq_start,
	.next = lisa_param_feature_seq_next,
	.stop = lisa_param_feature_seq_stop,
	.show = lisa_param_feature_seq_show,
};

static int lisa_param_feature_open(struct inode *inode, struct file *file)
{
	if (file->f_mode & FMODE_READ) {
		struct feature_param_entry **entry;

		entry = __seq_open_private(file, &lisa_param_feature_seq_ops,
					   sizeof(entry));
		if (entry)
			*entry = inode->i_private;

		return entry ? 0 : -ENOMEM;
	}
	return 0;
}

#define MAX_BUF_SIZE 1024
static ssize_t lisa_param_feature_write(struct file *file,
					const char __user *buf, size_t count,
					loff_t *ppos)
{
	struct feature_param_entry *entry = file->f_inode->i_private;
	char *kbuf, *s, *sep;
	ssize_t done = 0;
	int ret;

	/*
	 * Don't modify the 'set_features' or any parameter value if the
	 * config is activated. The process is:
	 * - De-activate
	 * - Modify
	 * - Re-activate
	 */
	if (entry->cfg->activated) {
		pr_err("Config must be deactivated before any update.\n");
		return -EBUSY;
	}

	lisa_sb_lock(file->f_inode->i_sb);

	if (!(file->f_flags & O_APPEND))
		drain_feature_param_entry_value(&entry->list_values);

	kbuf = kmalloc(MAX_BUF_SIZE, GFP_KERNEL);
	if (!kbuf) {
		done = -ENOMEM;
		goto leave;
	}

	while (done < count) {
		ssize_t size = count - done;

		if (size >= MAX_BUF_SIZE)
			size = MAX_BUF_SIZE - 1;

		if (copy_from_user(kbuf, buf + done, size)) {
			kfree(kbuf);
			goto done;
		}
		kbuf[size] = '\0';
		s = sep = kbuf;
		do  {
			sep = strchr(s, ',');
			if (sep) {
				*sep = '\0';
				++sep;
				++done;
			}
			done += size = strlen(s);
			/* skip leading whitespaces */
			while (isspace(*s) && *(s++))
				--size;
			if (!*s)
				goto next;
			if (done < count && !sep) {
				/* carry over ... */
				done -= strlen(s);
				goto next;
			}
			/* skip trailing whitespaces */
			while (size && isspace(s[--size]));
			if (strlen(s) > ++size)
				s[size] = '\0';

			ret = feature_param_add_new(entry, s);
			if (ret) {
				done = ret;
				goto done;
			}

			if (ppos)
				*ppos += 1;
		next:
			s = sep;
		} while (s);
	}
done:
	kfree(kbuf);
leave:
	lisa_sb_unlock(file->f_inode->i_sb);
	return done;
}

int lisa_param_feature_release(struct inode *inode, struct file *file)
{
	return file->f_mode & FMODE_READ ? seq_release_private(inode, file) : 0;
}

static struct file_operations lisa_param_feature_fops = {
	.owner = THIS_MODULE,
	.open = lisa_param_feature_open,
	.read = seq_read,
	.write = lisa_param_feature_write,
	.release = lisa_param_feature_release,
};

/////////////////////////////////////
//           configs
/////////////////////////////////////

/* TODO:
 * linux:
 * int (*mkdir) (struct mnt_idmap *, struct inode *,struct dentry *,
 *		      umode_t);
 * android:
 * int (*mkdir) (struct user_namespace *, struct inode *,struct dentry *,
 *                    umode_t);
 */

static int lisa_fs_mkdir(struct mnt_idmap *idmap, struct inode *inode,
			 struct dentry *dentry, umode_t mode)
{
	struct dentry *my_dentry;
	struct hlist_head *cfg_list;
	struct super_block *sb;
	struct lisa_cfg *cfg;
	int ret;

	sb = inode->i_sb;
	cfg_list = lisa_sb_get_cfg_list(sb);

	cfg = allocate_lisa_cfg(dentry->d_name.name);
	if (!cfg)
		return -ENOMEM;

	lisa_sb_lock(sb);

	my_dentry = lisa_fs_create_single(dentry->d_parent, dentry->d_name.name,
					  &simple_dir_inode_operations,
					  &simple_dir_operations,
					  S_IFDIR | mode, cfg);
	if (!my_dentry) {
		ret = -ENOMEM;
		goto error;
	}

	init_lisa_cfg(cfg, cfg_list, my_dentry);

	ret = lisa_fs_create_files(my_dentry, cfg);
	if (ret)
		goto error;

	lisa_sb_unlock(sb);
	return 0;

error:
	free_lisa_cfg(cfg);
	lisa_sb_unlock(sb);
	return ret;
}

void lisa_fs_remove(struct dentry *dentry)
{
	d_genocide(dentry);
}

static int lisa_fs_rmdir(struct inode *inode, struct dentry *dentry)
{
	struct hlist_head *cfg_list;
	struct super_block *sb;
	struct lisa_cfg *cfg;

	sb = inode->i_sb;
	cfg = inode->i_private;

	inode_unlock(inode);
	inode_unlock(d_inode(dentry));

	lisa_sb_lock(inode->i_sb);

	cfg_list = lisa_sb_get_cfg_list(sb);
	cfg = find_lisa_cfg(cfg_list, dentry->d_name.name);
	if (!cfg)
		pr_err("Failed to find config: %s\n", dentry->d_name.name);
	else
		free_lisa_cfg(cfg);

	lisa_sb_unlock(inode->i_sb);

	inode_lock_nested(inode, I_MUTEX_PARENT);
	inode_lock(d_inode(dentry));

	return 0;
}

const struct inode_operations lisa_fs_dir_inode_operations = {
	.lookup = simple_lookup,
	.mkdir = lisa_fs_mkdir,
	.rmdir = lisa_fs_rmdir,
};

/////////////////////////////////////
//           Main files
/////////////////////////////////////

static struct dentry *
lisa_fs_create_single(struct dentry *parent, const char *name,
		      const struct inode_operations *i_ops,
		      const struct file_operations *f_ops, umode_t mode,
		      void *data)
{
	struct dentry *dentry;
	struct inode *inode;

	dentry = d_alloc_name(parent, name);
	if (!dentry)
		return NULL;
	inode = lisa_fs_create_inode(parent->d_sb, mode);
	if (!inode) {
		dput(dentry);
		return NULL;
	}

	if (mode & S_IFREG) {
		inode->i_fop = f_ops;
	} else {
		inode->i_op = i_ops;
		inode->i_fop = f_ops;
	}
	inode->i_private = data;
	d_add(dentry, inode);
	if (mode & S_IFDIR) {
		inc_nlink(d_inode(parent));
		inc_nlink(inode);
	}

	return dentry;
}

/*
 * Note: Upon failure, the caller must call free_lisa_cfg(), which will go
 * over the elements of (struct lisa_cfg)->list_params and free them.
 * These elements are of the 'struct feature_param_entry' type.
 */
static int
lisa_fs_create_files(struct dentry *parent, struct lisa_cfg *cfg)
{
	struct feature_param_entry *entry;
	struct feature *feature;

	entry = allocate_feature_param_entry();
	if (!entry)
		return -ENOMEM;

	init_feature_param_entry(entry, cfg, &lisa_features_param);

	/* set_features: enable a feature - RW. */
	if (!lisa_fs_create_single(parent, "set_features",
			      NULL,
			      &lisa_param_feature_fops,
			      S_IFREG | S_IRUGO | S_IWUGO, entry))
		return -ENOMEM;

	/* available_features: list available features - RO. */
	if (!lisa_fs_create_single(parent, "available_features",
			      NULL,
			      &lisa_available_features_fops,
			      S_IFREG | S_IRUGO, &lisa_features_param))
		return -ENOMEM;

	/* activate: activate the selected (and configured) features - RW. */
	if (!lisa_fs_create_single(parent, "activate",
			      NULL,
			      &lisa_activate_fops,
			      S_IFREG | S_IRUGO | S_IWUGO, cfg))
		return -ENOMEM;

	/* configs: Dir containing configurations, only setup at the top/root level. */
	if (parent->d_sb->s_root == parent) {
		if (!lisa_fs_create_single(parent, "configs",
				&lisa_fs_dir_inode_operations,
				&simple_dir_operations,
				S_IFDIR | S_IRUGO, NULL))
			return -ENOMEM;
	}

	/* Create a dir for features having parameters. */
	for_each_feature(feature) {
		struct feature_param *param, **pparam;
		struct dentry *dentry;

		if (!feature->params)
			continue;

		dentry = lisa_fs_create_single(parent, feature->name,
					       &simple_dir_inode_operations,
					       &simple_dir_operations,
					       S_IFDIR | S_IRUGO, cfg);
		if (!dentry) {
			pr_err("Failed to initialize feature's (%s) root node\n",
			       feature->name);
			return -ENOMEM;
		}

		for_each_feature_param(param, pparam, feature) {
			entry = allocate_feature_param_entry();
			if (!entry)
				return -ENOMEM;

			init_feature_param_entry(entry, cfg, param);

			if (!lisa_fs_create_single(dentry, param->name, NULL,
						   &lisa_param_feature_fops,
						   S_IFREG | S_IRUGO, entry))
				return -ENOMEM;
		}
	}

	return 0;
}

/////////////////////////////////////
//           Super block
/////////////////////////////////////

static struct super_operations lisa_super_ops = {
	.statfs = simple_statfs,
};

static int lisa_fs_fill_super(struct super_block *sb, struct fs_context *fc)
{
	struct lisa_sb_info *lisa_info;
	struct lisa_cfg *cfg;
	struct inode *root;
	int ret = -ENOMEM;

	sb->s_maxbytes = MAX_LFS_FILESIZE;
	sb->s_blocksize = PAGE_SIZE;
	sb->s_blocksize_bits = PAGE_SHIFT;
	sb->s_magic = LISA_FS_SUPER_MAGIC;
	sb->s_op = &lisa_super_ops;

	lisa_info = kzalloc(sizeof(*lisa_info), GFP_KERNEL);
	if (!lisa_info)
		return -ENOMEM;

	mutex_init(&lisa_info->interface_lock);
	INIT_HLIST_HEAD(&lisa_info->cfg_list);
	sb->s_fs_info = lisa_info;

	root = lisa_fs_create_inode(sb, S_IFDIR | S_IRUGO);
	if (!root)
		goto error0;

	root->i_op = &simple_dir_inode_operations;
	root->i_fop = &simple_dir_operations;

	sb->s_root = d_make_root(root);
	if (!sb->s_root)
		goto error1;

	cfg = allocate_lisa_cfg("root");
	if (!cfg)
		goto error2;

	init_lisa_cfg(cfg, &lisa_info->cfg_list, sb->s_root);

	ret = lisa_fs_create_files(sb->s_root, cfg);
	if (ret)
		goto error3;

	return 0;

error3:
	free_lisa_cfg(cfg);
error2:
	dput(sb->s_root);
error1:
	iput(root);
error0:
	kfree(lisa_info);
	sb->s_fs_info = NULL;

	return ret;
}

static int lisa_fs_get_tree(struct fs_context *fc)
{
	return get_tree_single(fc, lisa_fs_fill_super);
}

static const struct fs_context_operations lisa_fs_context_ops = {
	.get_tree = lisa_fs_get_tree,
};

static int lisa_init_fs_context(struct fs_context *fc)
{
	fc->ops = &lisa_fs_context_ops;
	put_user_ns(fc->user_ns);
	fc->user_ns = get_user_ns(&init_user_ns);
	fc->global = true;
	return 0;
}

static void lisa_fs_kill_sb(struct super_block *sb)
{
	struct hlist_head *cfg_list;

	cfg_list = lisa_sb_get_cfg_list(sb);
	if (cfg_list)
		drain_lisa_cfg(cfg_list);

	kfree(sb->s_fs_info);
	sb->s_root = NULL;

	/*
	 * Free the lisa_features_param param,
	 * which is not bound to any feature.
	 */
	drain_feature_param_entry_value(&lisa_features_param.global_value);

	/* Free the inodes/dentries. */
	kill_litter_super(sb);
}

static struct file_system_type lisa_fs_type = {
	.owner = THIS_MODULE,
	.name = "lisa",
	.init_fs_context = lisa_init_fs_context,
	.kill_sb = lisa_fs_kill_sb,
};

int init_lisa_fs(void)
{
	int ret;

	ret = sysfs_create_mount_point(fs_kobj, "lisa");
	if (ret)
		goto error;

	ret = register_filesystem(&lisa_fs_type);
	if (ret) {
		sysfs_remove_mount_point(fs_kobj, "lisa");
		goto error;
	}

	return ret;

error:
	pr_err("Could not install lisa fs.\n");
	return ret;
}

void exit_lisa_fs(void)
{
	unregister_filesystem(&lisa_fs_type);
	sysfs_remove_mount_point(fs_kobj, "lisa");
}