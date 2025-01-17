# SPDX-License-Identifier: Apache-2.0
#
# Copyright (C) 2018, Arm Limited and contributors.
#
# Licensed under the Apache License, Version 2.0 (the "License"); you may
# not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
# WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

import math
import json
import re
from collections import OrderedDict
from shlex import quote
import copy
import itertools
import weakref
from statistics import mean
from operator import itemgetter

from devlib import TargetStableError

from lisa.wlgen.workload import Workload
from lisa.utils import Loggable, ArtifactPath, TASK_COMM_MAX_LEN, group_by_value, nullcontext, value_range
from lisa.pelt import PELT_SCALE


class CalibrationError(RuntimeError):
    """
    Exception raised when the ``rt-app`` calibration is not consistent with the
    CPU capacities in a way or another.
    """
    pass


class RTA(Workload):
    """
    An rt-app workload

    :param json_file: Path to the rt-app json description
    :type json_file: str

    .. warning::
      The class constructor only deals with pre-constructed json files.
      For creating rt-app workloads through other means, see :meth:`by_profile`
      and :meth:`by_str`.

    For more information about rt-app itself, see
    https://github.com/scheduler-tools/rt-app
    """

    required_tools = Workload.required_tools + ['rt-app']

    sched_policies = ['OTHER', 'FIFO', 'RR', 'DEADLINE']

    ALLOWED_TASK_NAME_REGEX = r'^[a-zA-Z0-9_]+$'

    def __init__(self, target, name=None, res_dir=None, json_file=None):
        # Don't add code here, use the early/late init methods instead.
        # This lets us factorize some code for the class methods that serve as
        # alternate constructors.

        self._early_init(target, name, res_dir, json_file)
        self._late_init()

    def _early_init(self, target, name, res_dir, json_file, log_stats=False, trace_events=None):
        """
        Initialize everything that is not related to the contents of the json file
        """
        super().__init__(target, name, res_dir)
        self.log_stats = log_stats
        self.trace_events = trace_events or []

        if not json_file:
            json_file = f'{self.name}.json'

        self.local_json = ArtifactPath.join(self.res_dir, json_file)
        self.remote_json = self.target.path.join(self.run_dir, json_file)

        rta_cmd = self.target.which('rt-app')
        if not rta_cmd:
            raise RuntimeError("No rt-app executable found on the target")

        self.command = f'{quote(rta_cmd)} {quote(self.remote_json)} 2>&1'

    def _late_init(self, calibration=None, tasks_names=None):
        """
        Complete initialization with a ready json file

        :parameters: Attributes that have been pre-computed and ended up
          in the json file. Passing them can prevent a needless file read.
        """
        if calibration or not tasks_names:
            with open(self.local_json) as fh:
                desc = json.load(fh)

                if calibration is None:
                    calibration = desc["global"]["calibration"]
                if not tasks_names:
                    tasks_names = list(desc["tasks"].keys())

        self.calibration = calibration
        self.tasks = sorted(tasks_names)

        # Move configuration file to target
        self.target.push(self.local_json, self.remote_json)

    def run(self, cpus=None, cgroup=None, as_root=False, update_cpu_capacities=None):
        logger = self.get_logger()
        plat_info = self.target.plat_info
        writeable_capacities = plat_info['cpu-capacities']['writeable']

        if update_cpu_capacities:
            if not writeable_capacities:
                raise ValueError('CPU capacities are not writeable on this target, please use update_cpu_capacities=False or None')
        # If left to None, we update if possible
        elif update_cpu_capacities is None:
            update_cpu_capacities = writeable_capacities
            if not writeable_capacities:
                logger.warning('CPU capacities will not be updated on this platform')

        if update_cpu_capacities:
            rtapp_capacities = plat_info['cpu-capacities']['rtapp']
            logger.info(f'Will update CPU capacities in sysfs: {rtapp_capacities}')

            write_kwargs = [
                dict(
                    path=f'/sys/devices/system/cpu/cpu{cpu}/cpu_capacity',
                    value=capa,
                    verify=True,
                )
                for cpu, capa in sorted(rtapp_capacities.items())
            ]
            capa_cm = self.target.batch_revertable_write_value(write_kwargs)
        else:
            # There might not be any rtapp calibration available, specifically
            # when we are being called to run the calibration workload.
            try:
                rtapp_capacities = plat_info['cpu-capacities']['rtapp']
                orig_capacities = plat_info['cpu-capacities']['orig']
            except KeyError:
                pass
            else:
                # Spit out some warning in case we are not going to update the
                # capacities, so we know what to expect
                RTA.warn_capacities_mismatch(orig_capacities, rtapp_capacities)

            capa_cm = nullcontext()

        with capa_cm:
            super().run(cpus, cgroup, as_root)

        if self.log_stats:
            logger.debug(f'Pulling logfiles to: {self.res_dir}')
            for task in self.tasks:
                # RT-app appends some number to the logs, so we can't predict the
                # exact filename
                logfile = self.target.path.join(self.run_dir, f'*{task}*.log')
                self.target.pull(logfile, self.res_dir, globbing=True)

    def _process_calibration(self, calibration):
        """
        Select CPU or pload value for task calibration
        """
        # This is done at init time rather than at run time, because the
        # calibration value lives in the file
        if isinstance(calibration, int):
            pass
        elif isinstance(calibration, str):
            calibration = calibration.upper()
        elif calibration is None:
            calib_map = self.target.plat_info['rtapp']['calib']
            calibration = min(calib_map.values())
        else:
            raise ValueError(f'Calibration value "{calibration}" is cannot be handled')

        return calibration

    @classmethod
    def by_profile(cls, target, profile, name=None, res_dir=None, default_policy=None,
                   max_duration_s=None, calibration=None,
                   log_stats=False, trace_events=None):
        """
        Create an rt-app workload using :class:`RTATask` instances

        :param profile: The workload description in a {task_name : :class:`RTATask`}
          shape
        :type profile: dict

        :param default_policy: Default scheduler policy. See :attr:`sched_policies`
        :type default_policy: str

        :param max_duration_s: Maximum duration of the workload. Will be determined
          by the longest running task if not specified.
        :type max_duration_s: int

        :param calibration: The calibration value to be used by rt-app. This can
          be an integer value or a CPU string (e.g. "CPU0").
        :type calibration: int or str

        :param log_stats: Generate a log file with stats for each task
        :type log_stats: bool

        :param trace_events: A list of trace events to generate.
            For a full list of trace events which can be generated by rt-app,
            refer to the tool documentation:
            https://github.com/scheduler-tools/rt-app/blob/master/doc/tutorial.txt
            By default, no events are generated.
        :type trace_events: list(str)

        A simple profile workload would be::

            task = Periodic(duty_cycle_pct=5)
            rta = RTA.by_profile(target, {"foo" : task})
            rta.run()
        """
        logger = cls.get_logger()
        self = cls.__new__(cls)
        self._early_init(target, name, res_dir, None, log_stats=log_stats,
                        trace_events=trace_events)

        # Sanity check for task names rt-app uses pthread_setname_np(3) which
        # limits the task name to 16 characters including the terminal '\0' and
        # the rt-app suffix.
        max_size = TASK_COMM_MAX_LEN - len('-XX-XXXX')
        too_long_tids = sorted(
            tid for tid in profile.keys()
            if len(tid) > max_size
        )
        if too_long_tids:
            raise ValueError(
                f'Task names too long, please configure your tasks with names shorter than {max_size} characters: {too_long_tids}')

        invalid_tids = sorted(
            tid for tid in profile.keys()
            if not re.match(cls.ALLOWED_TASK_NAME_REGEX, tid)
        )
        if invalid_tids:
            raise ValueError(
                f'Task names not matching "{cls.ALLOWED_TASK_NAME_REGEX}": {invalid_tids}')

        rta_profile = {
            # Keep a stable order for tasks definition, to get stable IDs
            # allocated by rt-app
            'tasks': OrderedDict(),
            'global': {}
        }

        calibration = self._process_calibration(calibration)

        global_conf = {
            'default_policy': 'SCHED_OTHER',
            'duration': -1 if not max_duration_s else max_duration_s,
            'calibration': calibration,
            # TODO: this can only be enabled when rt-app is running as root.
            # unfortunately, that's currently decided when calling
            # run(as_root=True), at which point we already generated and pushed
            # the JSON
            'lock_pages': False,
            'log_size': 'file' if log_stats else 'disable',
            'ftrace': ','.join(self.trace_events),
        }

        if max_duration_s:
            logger.warning(f'Limiting workload duration to {max_duration_s} [s]')

        if default_policy:
            if default_policy in self.sched_policies:
                global_conf['default_policy'] = f'SCHED_{default_policy}'
            else:
                raise ValueError(f'scheduling class {default_policy} not supported')

        logger.info(f"Calibration value: {global_conf['calibration']}")
        logger.info(f"Default policy: {global_conf['default_policy']}")

        rta_profile['global'] = global_conf

        # Setup tasks parameters
        for tid, task in sorted(profile.items(), key=itemgetter(0)):
            task_conf = {}

            if not task.sched_policy:
                task_conf['policy'] = global_conf['default_policy']
                sched_descr = 'sched: using default policy'
            else:
                task_conf['policy'] = f'SCHED_{task.sched_policy}'
                if task.priority is not None:
                    task_conf['prio'] = task.priority
                sched_descr = f'sched: {task.sched_policy}'

            logger.info('------------------------')
            logger.info(f'task [{tid}], {sched_descr}')

            task_conf['delay'] = int(task.delay_s * 1e6)
            logger.info(f' | start delay: {task.delay_s:.6f} [s]')

            task_conf['loop'] = task.loops
            logger.info(f' | loops count: {task.loops}')

            task_conf['phases'] = OrderedDict()
            rta_profile['tasks'][tid] = task_conf

            for pid, phase in enumerate(task.phases, start=1):
                phase_name = f'phase_{pid:0>6}'

                logger.info(f' + {phase_name}')
                rta_profile['tasks'][tid]['phases'][phase_name] = phase.get_rtapp_repr(tid, plat_info=target.plat_info)

        # Generate JSON configuration on local file
        with open(self.local_json, 'w') as outfile:
            json.dump(rta_profile, outfile, indent=4, separators=(',', ': '))
            outfile.write('\n')

        self._late_init(calibration=calibration,
                        tasks_names=list(profile.keys()))
        return self

    @classmethod
    def process_template(cls, template, duration=None, pload=None, log_dir=None,
                         work_dir=None):
        """
        Turn a raw string rt-app description into a JSON dict.
        Also, process some tokens and replace them.

        :param template: The raw string to process
        :type template: str

        :param duration: The value to replace ``__DURATION__`` with
        :type duration: int

        :param pload: The value to replace ``__PVALUE__`` with
        :type pload: int or str

        :param log_dir: The value to replace ``__LOGDIR__`` with
        :type log_dir: str

        :param work_dir: The value to replace ``__WORKDIR__`` with
        :type work_dir: str

        :returns: a JSON dict
        """

        replacements = {
            '__DURATION__': duration,
            '__PVALUE__': pload,
            '__LOGDIR__': log_dir,
            '__WORKDIR__': work_dir,
        }

        json_str = template
        for placeholder, value in replacements.items():
            if placeholder in template and placeholder is None:
                raise ValueError(f'Missing value for {placeholder} placeholder')
            else:
                json_str = json_str.replace(placeholder, json.dumps(value))

        return json.loads(json_str)

    @classmethod
    def by_str(cls, target, str_conf, name=None, res_dir=None, max_duration_s=None,
               calibration=None):
        """
        Create an rt-app workload using a pure string description

        :param str_conf: The raw string description. This must be a valid json
          description, with the exception of some tokens (see
          :meth:`process_template`) that will be replaced automagically.
        :type str_conf: str

        :param max_duration_s: Maximum duration of the workload.
        :type max_duration_s: int

        :param calibration: The calibration value to be used by rt-app. This can
          be an integer value or a CPU string (e.g. "CPU0").
        :type calibration: int or str
        """

        self = cls.__new__(cls)
        self._early_init(target, name, res_dir, None)

        calibration = self._process_calibration(calibration)

        json_conf = self.process_template(
            str_conf, max_duration_s, calibration, self.run_dir, self.run_dir)

        with open(self.local_json, 'w') as fh:
            json.dump(json_conf, fh)

        tasks_names = [tid for tid in json_conf['tasks']]
        self._late_init(calibration=calibration, tasks_names=tasks_names)

        return self

    @classmethod
    def _calibrate(cls, target, res_dir):
        res_dir = res_dir if res_dir else target .get_res_dir(
            "rta_calib", symlink=False
        )

        pload_regexp = re.compile(r'pLoad = ([0-9]+)ns')
        pload = {}

        logger = cls.get_logger()

        # Create calibration task
        if target.is_rooted:
            max_rtprio = int(target.execute('ulimit -Hr').splitlines()[0])
            logger.debug(f'Max RT prio: {max_rtprio}')

            priority = max_rtprio + 1 if max_rtprio <= 10 else 10
            sched_policy = 'FIFO'
        else:
            logger.warning('Will use default scheduler class instead of RT since the target is not rooted')
            priority = None
            sched_policy = None

        for cpu in target.list_online_cpus():
            logger.info(f'CPU{cpu} calibration...')

            # RT-app will run a calibration for us, so we just need to
            # run a dummy task and read the output
            calib_task = Periodic(
                duty_cycle_pct=100,
                duration_s=0.001,
                period_ms=1,
                priority=priority,
                sched_policy=sched_policy,
            )
            rta = cls.by_profile(target, name=f"rta_calib_cpu{cpu}",
                                 profile={'task1': calib_task},
                                 calibration=f"CPU{cpu}",
                                 res_dir=res_dir)

            with rta, target.freeze_userspace():
                # Disable CPU capacities update, since that leads to infinite
                # recursion
                rta.run(as_root=target.is_rooted, update_cpu_capacities=False)

            for line in rta.output.split('\n'):
                pload_match = re.search(pload_regexp, line)
                if pload_match is None:
                    continue
                pload[cpu] = int(pload_match.group(1))
                logger.debug(f'>>> CPU{cpu}: {pload[cpu]}')

        # Avoid circular import issue
        from lisa.platforms.platinfo import PlatformInfo
        snippet_plat_info = PlatformInfo({
            'rtapp': {
                'calib': pload,
            },
        })
        logger.info(f'Platform info rt-app calibration configuration:\n{snippet_plat_info.to_yaml_map_str()}')

        plat_info = target.plat_info

        # Sanity check calibration values for asymmetric systems if we have
        # access to capacities
        try:
            orig_capacities = plat_info['cpu-capacities']['orig']
        except KeyError:
            return pload

        capa_ploads = {
            capacity: {cpu: pload[cpu] for cpu in cpus}
            for capacity, cpus in group_by_value(orig_capacities).items()
        }

        # Find the min pload per capacity level, i.e. the fastest detected CPU.
        # It is more likely to represent the right pload, as it has suffered
        # from less IRQ slowdown or similar disturbances that might be random.
        capa_pload = {
            capacity: min(ploads.values())
            for capacity, ploads in capa_ploads.items()
        }

        # Sort by capacity
        capa_pload_list = sorted(capa_pload.items())
        # unzip the list of tuples
        _, pload_list = zip(*capa_pload_list)

        # If sorting according to capa was not equivalent to reverse sorting
        # according to pload (small pload=fast cpu)
        if list(pload_list) != sorted(pload_list, reverse=True):
            raise CalibrationError('Calibration values reports big cores less capable than LITTLE cores')

        # Check that the CPU capacities seen by rt-app are similar to the one
        # the kernel uses
        orig_capacities = plat_info['cpu-capacities']['orig']
        true_capacities = cls.get_cpu_capacities_from_calibrations(orig_capacities, pload)
        cls.warn_capacities_mismatch(orig_capacities, true_capacities)

        return pload

    @classmethod
    def warn_capacities_mismatch(cls, orig_capacities, new_capacities):
        """
        Compare ``orig_capacities`` and ``new_capacities`` and log warnings if
        they are not consistent.

        :param orig_capacities: Original CPU capacities, as a map of CPU to capacity.
        :type orig_capacities: dict(int, int)

        :param new_capacities: New CPU capacities, as a map of CPU to capacity.
        :type new_capacities: dict(int, int)
        """
        logger = cls.get_logger()
        capacities = {
            cpu: (orig_capacities[cpu], new_capacities[cpu])
            for cpu in orig_capacities.keys() & new_capacities.keys()
        }
        logger.info(f'CPU capacities according to rt-app workload: {new_capacities}')

        capa_factors_pct = {
            cpu: new / orig * 100
            for cpu, (orig, new) in capacities.items()
        }
        dispersion_pct = max(abs(100 - factor) for factor in capa_factors_pct.values())

        if dispersion_pct > 2:
            logger.warning(f'The calibration values are not inversely proportional to the CPU capacities, the duty cycles will be up to {dispersion_pct:.2f}% off on some CPUs: {capa_factors_pct}')

        if dispersion_pct > 20:
            logger.warning(f'The calibration values are not inversely proportional to the CPU capacities. Either rt-app calibration failed, or the rt-app busy loops has a very different instruction mix compared to the workload used to establish the CPU capacities: {capa_factors_pct}')

        # Map of CPUs X to list of CPUs Ys that are faster than it although CPUs
        # of Ys have a smaller orig capacity than X
        if len(capacities) > 1:
            faster_than_map = {
                cpu1: sorted(
                    cpu2
                    for cpu2, (orig2, new2) in capacities.items()
                    if new2 > new1 and orig2 < orig1
                )
                for cpu1, (orig1, new1) in capacities.items()
            }
        else:
            faster_than_map = {}

        # Remove empty lists
        faster_than_map = {
            cpu: faster_cpus
            for cpu, faster_cpus in faster_than_map.items()
            if faster_cpus
        }

        if faster_than_map:
            raise CalibrationError(f'Some CPUs of higher capacities are slower than other CPUs of smaller capacities: {faster_than_map}')

    @classmethod
    def get_cpu_capacities_from_calibrations(cls, orig_capacities, calibrations):
        """
        Compute the CPU capacities out of the rt-app calibration values.

        :returns: A mapping of CPU to capacity.

        :param orig_capacities: Original capacities as a mapping of CPU ID to
            capacity.
        :type orig_capacities: dict(int, int)

        :param calibrations: Mapping of CPU to pload value.
        :type calibrations: dict
        """

        # calibration values are inversely proportional to the CPU capacities
        inverse_calib = {cpu: 1 / calib for cpu, calib in calibrations.items()}

        def compute_capa(cpu):
            # True CPU capacity for the rt-app workload, rather than for the
            # whatever workload was used to compute the CPU capacities exposed by
            # the kernel
            return inverse_calib[cpu] / max(inverse_calib.values()) * PELT_SCALE

        rtapp_capacities =  {cpu: compute_capa(cpu) for cpu in calibrations.keys()}

        # Average in a capacity class, since the kernel will only use one
        # value for the whole class anyway
        new_capacities = {}
        # Group the CPUs by original capacity
        for capa, capa_class in group_by_value(orig_capacities).items():
            avg_capa = mean(
                capa
                for cpu, capa in rtapp_capacities.items()
                if cpu in capa_class
            )
            new_capacities.update({cpu: avg_capa for cpu in capa_class})

        # Make sure that the max cap is 1024 and that we use integer values
        new_max_cap = max(new_capacities.values())
        new_capacities = {
            # Make sure the max cap will be 1024 and not 1023 due to rounding
            # errors
            cpu: math.ceil(capa / new_max_cap * 1024)
            for cpu, capa in new_capacities.items()
        }
        return new_capacities


    @classmethod
    def get_cpu_calibrations(cls, target, res_dir=None):
        """
        Get the rt-ap calibration value for all CPUs.

        :param target: Target to run calibration on.
        :type target: lisa.target.Target

        :returns: Dict mapping CPU numbers to rt-app calibration values.
        """

        if not target.is_module_available('cpufreq'):
            cls.get_logger().warning(
                'cpufreq module not loaded, skipping setting frequency to max')
            cm = nullcontext()
        else:
            cm = target.cpufreq.use_governor('performance')

        with cm, target.disable_idle_states():
            return cls._calibrate(target, res_dir)

    @classmethod
    def _compute_task_map(cls, trace, names):
        prefix_regexps = {
            prefix: re.compile(rf"^{re.escape(prefix)}(-[0-9]+)*$")
            for prefix in names
        }

        task_map = {
            prefix: sorted(
                task_id
                for task_id in trace.task_ids
                if re.match(regexp, task_id.comm)
            )
            for prefix, regexp in prefix_regexps.items()
        }

        missing = sorted(prefix for prefix, task_ids in task_map.items() if not task_ids)
        if missing:
            raise RuntimeError(f"Missing tasks matching the following rt-app profile names: {', '.join(missing)}")
        return task_map

    # Mapping of Trace objects to their task map.
    # We don't want to keep traces alive just for this cache, so we use weak
    # references for the keys.
    _traces_task_map = weakref.WeakKeyDictionary()

    @classmethod
    def resolve_trace_task_names(cls, trace, names):
        """
        Translate an RTA profile task name to a list of
        :class:`lisa.trace.TaskID` as found in a :class:`lisa.trace.Trace`.

        :returns: A dictionnary of ``rt-app`` profile names to list of
            :class:`lisa.trace.TaskID` The list will contain more than one item
            if the task forked.

        :param trace: Trace to look at.
        :type trace: lisa.trace.Trace

        :param names: ``rt-app`` task names as specified in profile keys
        :type names: list(str)
        """

        task_map = cls._traces_task_map.setdefault(trace, {})
        # Update with the names that have not been discovered yet
        not_computed_yet = set(names) - task_map.keys()
        if not_computed_yet:
            task_map.update(cls._compute_task_map(trace, not_computed_yet))

        # Only return what we were asked for, so the client code does not
        # accidentally starts depending on whatever was requested in earlier
        # calls
        return {
            name: task_ids
            for name, task_ids in task_map.items()
            if name in names
        }

    def get_trace_task_names(self, trace):
        """
        Get a dictionnary of :class:`lisa.trace.TaskID` used in the given trace
        for this task.
        """
        return self.resolve_trace_task_names(trace, self.tasks)


class Phase(Loggable):
    """
    Descriptor for an rt-app load phase

    :param duration_s: the phase duration in [s].
    :type duration_s: float

    :param period_ms: the phase period in [ms].
    :type period_ms: float

    :param duty_cycle_pct: the generated load in percents.
    :type duty_cycle_pct: float

    :param cpus: the CPUs on which task execution is restricted during this phase.
        If unspecified, that phase will be allowed to run on any CPU,
        regardless of the affinity of the previous phases.
    :type cpus: list(int) or None

    :param barrier_after: if provided, the name of the barrier to sync against
                          when reaching the end of this phase. Currently only
                          supported when duty_cycle_pct=100
    :type barrier_after: str

    :param uclamp_min: the task uclamp.min value to set for the task for the
      duration of the phase.
    :type uclamp_min: int

    :param uclamp_max: the task uclamp.max value to set for the task for the
      duration of the phase.
    :type uclamp_max: int

    :param numa_nodes_membind: the list of NUMA Nodes.
        Task will only allocate memory from these nodes during this phase.
        If unspecified, that phase will be allowed to allocate memory from any
        NUMA node, regardless of the previous phase settings.
    :type numa_nodes_membind: list(int) or None
    """

    def __init__(self, duration_s, period_ms, duty_cycle_pct, cpus=None, barrier_after=None,
                 uclamp_min=None, uclamp_max=None, numa_nodes_membind=None):
        if barrier_after and duty_cycle_pct != 100:
            # This could be implemented but currently don't foresee any use.
            raise ValueError('Barriers only supported when duty_cycle_pct=100')

        self.duration_s = duration_s
        self.period_ms = period_ms
        self.duty_cycle_pct = duty_cycle_pct
        self.cpus = cpus
        self.barrier_after = barrier_after
        self.uclamp_min = uclamp_min
        self.uclamp_max = uclamp_max
        self.numa_nodes_membind = numa_nodes_membind

    def get_rtapp_repr(self, task_name, plat_info):
        """
        Get a dictionnary representation of the phase as expected by rt-app

        :param task_name: Name of the phase's task (needed for timers)
        :type task_name: str

        :param plat_info: Platform info of the target that is going to be used
            to run the phase.
        :type plat_info: lisa.platforms.platinfo.PlatformInfo

        :returns: OrderedDict
        """
        logger = self.get_logger()
        phase = OrderedDict()
        # Convert time parameters to integer [us] units
        duration = int(self.duration_s * 1e6)

        # A duty-cycle of 0[%] translates to a 'sleep' phase
        if self.duty_cycle_pct == 0:
            logger.info(' | sleep {:.6f} [s]'.format(duration / 1e6))

            phase['loop'] = 1
            phase['sleep'] = duration

        # A duty-cycle of 100[%] translates to a 'run-only' phase
        elif self.duty_cycle_pct == 100:
            logger.info(' | batch {:.6f} [s]'.format(duration / 1e6))

            phase['loop'] = 1
            phase['run'] = duration
            if self.barrier_after:
                phase['barrier'] = self.barrier_after

        # A certain number of loops is requires to generate the
        # proper load
        else:
            period = int(self.period_ms * 1e3)

            cloops = -1
            if duration >= 0:
                cloops = duration // period

            sleep_time = period * (100 - self.duty_cycle_pct) // 100
            # rtapp fails to handle floating values correctly
            # https://github.com/scheduler-tools/rt-app/issues/82
            running_time = int(period - sleep_time)

            logger.info(' | duration {:.6f} [s] ({} loops)'.format(
                        duration / 1e6, cloops))
            logger.info(f' |  period   {int(period):>3} [us], duty_cycle {self.duty_cycle_pct:>3,.2f} %')
            logger.info(f' |  run_time {int(running_time):>6} [us], sleep_time {int(sleep_time):>6} [us]')

            phase['loop'] = cloops
            phase['run'] = running_time
            phase['timer'] = {'ref': task_name, 'period': period}

        # Set the affinity to all CPUs in the system, i.e. do not set any affinity
        if self.cpus is None:
            cpus = list(range(plat_info['cpus-count']))
        else:
            cpus = self.cpus
        phase['cpus'] = cpus

        if self.uclamp_min is not None:
            phase['util_min'] = self.uclamp_min
            logger.info(f' | util_min {self.uclamp_min:>7}')

        if self.uclamp_max is not None:
            phase['util_max'] = self.uclamp_max
            logger.info(f' | util_max {self.uclamp_max:>7}')

        # Allow memory allocation from all NUMA nodes in the system
        if self.numa_nodes_membind is None:
            nodes_membind = list(range(plat_info['numa-nodes-count']))
        else:
            nodes_membind = self.numa_nodes_membind
        phase['nodes_membind'] = nodes_membind

        return phase

class RTATask:
    """
    Base class for conveniently constructing params to :meth:`RTA.by_profile`

    :param delay_s: the delay in seconds before starting.
    :type delay_s: float

    :param loops: Number of times to repeat the described task (including
      initial delay). -1 indicates infinite looping
    :type loops: int

    :param sched_policy: the scheduler policy for this task. Defaults to
      ``SCHED_OTHER``, see :manpage:`sched` for information on scheduler policies.
    :type sched_policy: str or None

    :param priority: the scheduler priority for this task. See :manpage:`sched`
      for information on scheduler priorities.
    :type priority: int or None

    This class represents an rt-app task which may contain multiple :class:`Phase`.
    It implements ``__add__`` so that using ``+`` on two tasks concatenates their
    phases. For example ``Ramp() + Periodic()`` would yield an ``RTATask`` that
    executes the default phases for :class:`Ramp` followed by the default phases for
    :class:`Periodic`.
    """

    def __init__(self, delay_s=0, loops=1, sched_policy=None, priority=None):
        self.delay_s = delay_s
        self.loops = loops

        if isinstance(sched_policy, str):
            sched_policy = sched_policy.upper()

            if sched_policy not in RTA.sched_policies:
                raise ValueError(f'scheduling class {sched_policy} not supported')

        self.sched_policy = sched_policy
        self.priority = priority
        self.phases = []

    def __add__(self, task):
        # Do not modify the original object which might still be used for other
        # purposes
        new = copy.deepcopy(self)
        # Piggy back on the __iadd__ implementation
        new += task
        return new

    def __iadd__(self, task):
        if task.delay_s:
            # This won't work, because rt-app's "delay" field is per-task and
            # not per-phase. We might be able to implement it by adding a
            # "sleep" event here, but let's not bother unless such a need
            # arises.
            raise ValueError("Can't compose rt-app tasks "
                             "when the second has nonzero 'delay_s'")
        self.phases.extend(task.phases)
        return self


class Ramp(RTATask):
    """
    Configure a ramp load.

    This class defines a task which load is a ramp with a configured number
    of steps according to the input parameters.

    :param start_pct: the initial load percentage.
    :type start_pct: float

    :param end_pct: the final load percentage.
    :type end_pct: float

    :param delta_pct: the load increase/decrease at each step, in percentage
      points.
    :type delta_pct: float

    :param time_s: the duration in seconds of each load step.
    :type time_s: float

    :param period_ms: the period used to define the load in [ms].
    :type period_ms: float

    .. seealso:: See :class:`RTATask` for the documentation of the following
      parameters:

      * **delay_s**
      * **loops**
      * **sched_policy**
      * **priority**

    .. seealso:: See :class:`Phase` for the documentation of the following
      parameters:

      * **cpus**
      * **uclamp_min**
      * **uclamp_max**
      * **numa_nodes_membind**
    """

    def __init__(self, start_pct=0, end_pct=100, delta_pct=10, time_s=1,
                 period_ms=100, delay_s=0, loops=1, sched_policy=None,
                 priority=None, cpus=None, uclamp_min=None, uclamp_max=None,
                 numa_nodes_membind=None):
        super().__init__(delay_s, loops, sched_policy, priority)

        if not (0 <= start_pct <= 100 and 0 <= end_pct <= 100):
            raise ValueError('start_pct and end_pct must be in [0..100] range')

        # Make sure the delta goes in the right direction
        sign = +1 if start_pct <= end_pct else -1
        delta_pct = sign * abs(delta_pct)

        steps = list(value_range(start_pct, end_pct, delta_pct, inclusive=True))
        # clip the last step
        steps[-1] = end_pct

        phases = []
        for load in steps:
            if load == 0:
                phase = Phase(time_s, 0, 0, cpus, uclamp_min=uclamp_min,
                              uclamp_max=uclamp_max, numa_nodes_membind=numa_nodes_membind)
            else:
                phase = Phase(time_s, period_ms, load, cpus,
                              uclamp_min=uclamp_min, uclamp_max=uclamp_max,
                              numa_nodes_membind=numa_nodes_membind)
            phases.append(phase)

        self.phases = phases


class Step(Ramp):
    """
    Configure a step load.

    This class defines a task which load is a step with a configured initial and
    final load. Using the ``loops`` param, this can be used to create a workload
    that alternates between two load values.

    :param start_pct: the initial load percentage.
    :type start_pct: float

    :param end_pct: the final load percentage.
    :type end_pct: float

    :param time_s: the duration in seconds of each load step.
    :type time_s: float

    :param period_ms: the period used to define the load in [ms].
    :type period_ms: float

    .. seealso:: See :class:`RTATask` for the documentation of the following
      parameters:

      * **delay_s**
      * **loops**
      * **sched_policy**
      * **priority**

    .. seealso:: See :class:`Phase` for the documentation of the following
      parameters:

      * **cpus**
      * **uclamp_min**
      * **uclamp_max**
      * **numa_nodes_membind**
    """

    def __init__(self, start_pct=0, end_pct=100, time_s=1, period_ms=100,
                 delay_s=0, loops=1, sched_policy=None, priority=None, cpus=None,
                 uclamp_min=None, uclamp_max=None, numa_nodes_membind=None):
        delta_pct = abs(end_pct - start_pct)
        super().__init__(start_pct, end_pct, delta_pct, time_s,
                         period_ms, delay_s, loops, sched_policy,
                         priority, cpus, uclamp_min, uclamp_max, numa_nodes_membind)


class Pulse(RTATask):
    """
    Configure a pulse load.

    This class defines a task which load is a pulse with a configured
    initial and final load.

    The main difference with the 'step' class is that a pulse workload is
    by definition a 'step down', i.e. the workload switch from an initial
    load to a final one which is always lower than the initial one.
    Moreover, a pulse load does not generate a sleep phase in case of 0[%]
    load, i.e. the task ends as soon as the non null initial load has
    completed.

    :param start_pct: the initial load percentage.
    :type start_pct: float

    :param end_pct: the final load percentage.
    :type end_pct: float

    :param time_s: the duration in seconds of each load step.
    :type time_s: float

    :param period_ms: the period used to define the load in [ms].
    :type period_ms: float

    .. seealso:: See :class:`RTATask` for the documentation of the following
      parameters:

      * **delay_s**
      * **loops**
      * **sched_policy**
      * **priority**

    .. seealso:: See :class:`Phase` for the documentation of the following
      parameters:

      * **cpus**
      * **uclamp_min**
      * **uclamp_max**
      * **numa_nodes_membind**
    """

    def __init__(self, start_pct=100, end_pct=0, time_s=1, period_ms=100,
                 delay_s=0, loops=1, sched_policy=None, priority=None, cpus=None,
                 uclamp_min=None, uclamp_max=None, numa_nodes_membind=None):
        super().__init__(delay_s, loops, sched_policy, priority)

        if end_pct > start_pct:
            raise ValueError('end_pct must be lower than start_pct')

        if not (0 <= start_pct <= 100 and 0 <= end_pct <= 100):
            raise ValueError('end_pct and start_pct must be in [0..100] range')

        loads = [start_pct]
        if end_pct:
            loads += [end_pct]

        self.phases = [
            Phase(time_s, period_ms, load, cpus, uclamp_min=uclamp_min,
                          uclamp_max=uclamp_max, numa_nodes_membind=numa_nodes_membind)
            for load in loads
        ]


class Periodic(Pulse):
    """
    Configure a periodic load. This is the simplest type of RTA task.

    This class defines a task which load is periodic with a configured
    period and duty-cycle.

    :param duty_cycle_pct: the generated load in percents.
    :type duty_cycle_pct: float

    :param duration_s: the phase duration in [s].
    :type duration_s: float

    :param period_ms: the period used to define the load in [ms].
    :type period_ms: float

    .. seealso:: See :class:`RTATask` for the documentation of the following
      parameters:

      * **delay_s**
      * **loops**
      * **sched_policy**
      * **priority**

    .. seealso:: See :class:`Phase` for the documentation of the following
      parameters:

      * **cpus**
      * **uclamp_min**
      * **uclamp_max**
      * **numa_nodes_membind**
    """

    def __init__(self, duty_cycle_pct=50, duration_s=1, period_ms=100,
                 delay_s=0, sched_policy=None, priority=None, cpus=None,
                 uclamp_min=None, uclamp_max=None, numa_nodes_membind=None):
        super().__init__(duty_cycle_pct, 0, duration_s,
                         period_ms, delay_s, 1, sched_policy,
                         priority, cpus,
                         uclamp_min=uclamp_min,
                         uclamp_max=uclamp_max,
                         numa_nodes_membind=numa_nodes_membind)


class RunAndSync(RTATask):
    """
    Configure a task that runs 100% then waits on a barrier

    :param barrier: name of barrier to wait for. Sleeps until any other tasks
      that refer to this barrier have reached the barrier too.
    :type barrier: str

    :param time_s: time to run for in [s]
    :type time_s: float

    .. seealso:: See :class:`RTATask` for the documentation of the following
      parameters:

      * **delay_s**
      * **loops**
      * **sched_policy**
      * **priority**

    .. seealso:: See :class:`Phase` for the documentation of the following
      parameters:

      * **cpus**
      * **uclamp_min**
      * **uclamp_max**
      * **numa_nodes_membind**
    """

    def __init__(self, barrier, time_s=1, delay_s=0, loops=1, sched_policy=None,
                 priority=None, cpus=None, uclamp_min=None, uclamp_max=None, numa_nodes_membind=None):
        super().__init__(delay_s, loops, sched_policy, priority)

        # This should translate into a phase containing a 'run' event and a
        # 'barrier' event
        self.phases = [Phase(time_s, None, 100, cpus, barrier_after=barrier,
                             uclamp_min=uclamp_min, uclamp_max=uclamp_max, numa_nodes_membind=numa_nodes_membind)]

# vim :set tabstop=4 shiftwidth=4 textwidth=80 expandtab
