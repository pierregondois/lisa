#    Copyright 2015-2016 ARM Limited
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

"""Scheduler specific Functionality for the
stats framework

The Scheduler stats aggregation is based on a signal
which is generated by the combination of two triggers
from the events with the following parameters

========================= ============ =============
EVENT                       VALUE          FILTERS
========================= ============ =============
:func:`sched_switch`        1           next_pid
:func:`sched_switch`      -1           prev_pid
========================= ============ =============

Both these Triggers are provided by the event
:mod:`trappy.sched.SchedSwitch` which correspond to
the :code:`sched_switch` unique word in the trace

.. seealso:: :mod:`trappy.stats.Trigger.Trigger`

Using the above information the following signals are
generated.

**EVENT SERIES**

This is a combination of the two triggers as specified
above and has alternating +/- 1 values and is merely
a representation of the position in time when the process
started or stopped running on a CPU

**RESIDENCY SERIES**

This series is a cumulative sum of the event series and
is a representation of the continuous residency of the
process on a CPU

The pivot for the aggregators is the CPU on which the
event occurred on. If N is the number of CPUs in the
system, N signal for each CPU are generated. These signals
can then be aggregated by specifying a Topology

.. seealso:: :mod:`trappy.stats.Topology.Topology`
"""
from __future__ import division
from __future__ import unicode_literals
from __future__ import print_function

import numpy as np
from trappy.stats.Trigger import Trigger

WINDOW_SIZE = 0.0001
"""A control config for filter events. Some analyses
may require ignoring of small interruptions"""

# Trigger Values
SCHED_SWITCH_IN = 1
"""Value of the event when a task is **switch in**
or scheduled on a CPU"""
SCHED_SWITCH_OUT = -1
"""Value of the event when a task is **switched out**
or relinquishes a CPU"""
NO_EVENT = 0
"""Signifies no event on an event trace"""

# Field Names
CPU_FIELD = "__cpu"
"""The column in the sched_switch event that
indicates the CPU on which the event occurred
"""
NEXT_PID_FIELD = "next_pid"
"""The column in the sched_switch event that
indicates the PID of the next process to be scheduled
"""
PREV_PID_FIELD = "prev_pid"
"""The column in the sched_switch event that
indicates the PID of the process that was scheduled
in
"""
TASK_RUNNING = 1
"""The column in the sched_switch event that
indicates the CPU on which the event occurred
"""
TASK_NOT_RUNNING = 0
"""In a residency series, a zero indicates
that the task is not running
"""
TIME_INVAL = -1
"""Standard Value to indicate invalid time data"""
SERIES_SANTIZED = "_sched_sanitized"
"""A memoized flag which is set when an event series
is checked for boundary conditions
"""


def sanitize_asymmetry(series, window=None):
    """Sanitize the cases when a :code:`SWITCH_OUT`
    happens before a :code:`SWITCH_IN`. (The case when
    a process is already running before the trace started)

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple
    """

    if not hasattr(series, SERIES_SANTIZED):

        events = series[series != 0]
        if len(series) >= 2 and len(events):
            if series.values[0] == SCHED_SWITCH_OUT:
                series.values[0] = TASK_NOT_RUNNING

            elif events.values[0] == SCHED_SWITCH_OUT:
                series.values[0] = SCHED_SWITCH_IN
                if window:
                    series.index.values[0] = window[0]

            if series.values[-1] == SCHED_SWITCH_IN:
                series.values[-1] = TASK_NOT_RUNNING

            elif events.values[-1] == SCHED_SWITCH_IN:
                series.values[-1] = SCHED_SWITCH_OUT
                if window:
                    series.index.values[-1] = window[1]

        # No point if the series just has one value and
        # one event. We do not have sufficient data points
        # for any calculation. We should Ideally never reach
        # here.
        elif len(series) == 1:
            series.values[0] = 0

        setattr(series, SERIES_SANTIZED, True)

    return series


def csum(series, window=None, filter_gaps=False):
    """:func:`aggfunc` for the cumulative sum of the
    input series data

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple

    :param filter_gaps: If set, a process being switched out
        for :mod:`bart.sched.functions.WINDOW_SIZE` is
        ignored. This is helpful when small interruptions need
        to be ignored to compare overall correlation
    :type filter_gaps: bool
    """

    if filter_gaps:
        series = filter_small_gaps(series)

    series = series.cumsum()
    return select_window(series, window)

def filter_small_gaps(series):
    """A helper function that does filtering of gaps
    in residency series < :mod:`bart.sched.functions.WINDOW_SIZE`

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`
    """

    start = None
    for index, value in series.items():

        if value == SCHED_SWITCH_IN:
            if start == None:
                continue

            if index - start < WINDOW_SIZE:
                series[start] = NO_EVENT
                series[index] = NO_EVENT
            start = None

        if value == SCHED_SWITCH_OUT:
            start = index

    return series

def first_cpu(series, window=None):
    """:func:`aggfunc` to calculate the time of
    the first switch in event in the series
    This is returned as a vector of unit length
    so that it can be aggregated and reduced across
    nodes to find the first cpu of a task

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple
    """
    series = select_window(series, window)
    series = series[series == SCHED_SWITCH_IN]
    if len(series):
        return [series.index.values[0]]
    else:
        return [float("inf")]

def last_cpu(series, window=None):
    """:func:`aggfunc` to calculate the time of
    the last switch out event in the series
    This is returned as a vector of unit length
    so that it can be aggregated and reduced across
    nodes to find the last cpu of a task

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple
    """
    series = select_window(series, window)
    series = series[series == SCHED_SWITCH_OUT]

    if len(series):
        return [series.index.values[-1]]
    else:
        return [0]

def select_window(series, window):
    """Helper Function to select a portion of
    pandas time series

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple
    """

    if not window:
        return series

    start, stop = window
    ix = series.index
    selector = ((ix >= start) & (ix <= stop))
    window_series = series[selector]
    return window_series

def residency_sum(series, window=None):
    """:func:`aggfunc` to calculate the total
    residency


    The input series is processed for
    intervals between a :mod:`bart.sched.functions.SCHED_SWITCH_OUT`
    and :mod:`bart.sched.functions.SCHED_SWITCH_IN` to track
    additive residency of a task

    .. math::

        S_{in} = i_{1}, i_{2}...i_{N} \\\\
        S_{out} = o_{1}, o_{2}...o_{N} \\\\
        R_{total} = \sum_{k}^{N}\Delta_k = \sum_{k}^{N}(o_{k} - i_{k})

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple

    :return: A scalar float value
    """

    if not len(series):
        return 0.0

    org_series = series
    series = select_window(series, window)
    series = sanitize_asymmetry(series, window)

    s_in = series[series == SCHED_SWITCH_IN]
    s_out = series[series == SCHED_SWITCH_OUT]

    if not (len(s_in) and len(s_out)):
        try:
            org_series = sanitize_asymmetry(org_series)
            running = select_window(org_series.cumsum(), window)
            if running.values[0] == TASK_RUNNING and running.values[-1] == TASK_RUNNING:
                return window[1] - window[0]
        except Exception as e:
            pass

    if len(s_in) != len(s_out):
        raise RuntimeError(
            "Unexpected Lengths: s_in={}, s_out={}".format(
                len(s_in),
                len(s_out)))
    else:
        return np.sum(s_out.index.values - s_in.index.values)


def first_time(series, value, window=None):
    """:func:`aggfunc` to:

    - Return the first index where the
      series == value

    - If no such index is found
      +inf is returned

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple

    :return: A vector of Unit Length
    """

    series = select_window(series, window)
    series = series[series == value]

    if not len(series):
        return [float("inf")]

    return [series.index.values[0]]


def period(series, align="start", window=None):
    """This :func:`aggfunc` returns a tuple
    of the average duration between two triggers:

        - When :code:`align=start` the :code:`SCHED_IN`
          trigger is used

        - When :code:`align=end` the :code:`SCHED_OUT`
          trigger is used


    .. math::

        E = e_{1}, e_{2}...e_{N} \\\\
        T_p = \\frac{\sum_{j}^{\lfloor N/2 \\rfloor}(e_{2j + 1} - e_{2j})}{N}

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple

    :return:
        A list of deltas of successive starts/stops
        of a task

    """

    series = select_window(series, window)
    series = sanitize_asymmetry(series, window)

    if align == "start":
        series = series[series == SCHED_SWITCH_IN]
    elif align == "end":
        series = series[series == SCHED_SWITCH_OUT]

    if len(series) % 2 == 0:
        series = series[:1]

    if not len(series):
        return []

    return list(np.diff(series.index.values))

def last_time(series, value, window=None):
    """:func:`aggfunc` to:

    - The first index where the
      series == value

    - If no such index is found
      :mod:`bart.sched.functions.TIME_INVAL`
      is returned

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple

    :return: A vector of Unit Length
    """

    series = select_window(series, window)
    series = series[series == value]
    if not len(series):
        return [TIME_INVAL]

    return [series.index.values[-1]]


def binary_correlate(series_x, series_y):
    """Helper function to Correlate binary Data

    Both the series should have same indices

    For binary time series data:

    .. math::

        \\alpha_{corr} = \\frac{N_{agree} - N_{disagree}}{N}

    :param series_x: First time Series data
    :type series_x: :mod:`pandas.Series`

    :param series_y: Second time Series data
    :type series_y: :mod:`pandas.Series`
    """

    if len(series_x) != len(series_y):
        raise ValueError("Cannot compute binary correlation for \
                          unequal vectors")

    agree = len(series_x[series_x == series_y])
    disagree = len(series_x[series_x != series_y])

    return (agree - disagree) / len(series_x)

def get_pids_for_process(ftrace, execname, cls=None):
    """Get the PIDs for a given process

    :param ftrace: A ftrace object with a sched_switch
        event
    :type ftrace: :mod:`trappy.ftrace.FTrace`

    :param execname: The name of the process
    :type execname: str

    :param cls: The SchedSwitch event class (required if
        a different event is to be used)
    :type cls: :mod:`trappy.base.Base`

    :return: The set of PIDs for the execname
    """

    if not cls:
        try:
            df = ftrace.sched_switch.data_frame
        except AttributeError:
            raise ValueError("SchedSwitch event not found in ftrace")

        if len(df) == 0:
            raise ValueError("SchedSwitch event not found in ftrace")
    else:
        event = getattr(ftrace, cls.name)
        df = event.data_frame

    mask = df["next_comm"].apply(lambda x : True if x == execname else False)
    return list(np.unique(df[mask]["next_pid"].values))

def get_task_name(ftrace, pid, cls=None):
    """Returns the execname for pid

    :param ftrace: A ftrace object with a sched_switch
        event
    :type ftrace: :mod:`trappy.ftrace.FTrace`

    :param pid: The PID of the process
    :type pid: int

    :param cls: The SchedSwitch event class (required if
        a different event is to be used)
    :type cls: :mod:`trappy.base.Base`

    :return: The execname for the PID
    """

    if not cls:
        try:
            df = ftrace.sched_switch.data_frame
        except AttributeError:
           raise ValueError("SchedSwitch event not found in ftrace")
    else:
        event = getattr(ftrace, cls.name)
        df = event.data_frame

    df = df[df["next_pid"] == pid]
    if not len(df):
        return ""
    else:
        return df["next_comm"].values[0]

def sched_triggers(ftrace, pid, sched_switch_class):
    """Returns the list of sched_switch triggers

    :param ftrace: A ftrace object with a sched_switch
        event
    :type ftrace: :mod:`trappy.ftrace.FTrace`

    :param pid: The PID of the associated process
    :type pid: int

    :param sched_switch_class: The SchedSwitch event class
    :type sched_switch_class: :mod:`trappy.base.Base`

    :return: List of triggers, such that
        ::

            triggers[0] = switch_in_trigger
            triggers[1] = switch_out_trigger
    """

    if not hasattr(ftrace, "sched_switch"):
        raise ValueError("SchedSwitch event not found in ftrace")

    triggers = []
    triggers.append(sched_switch_in_trigger(ftrace, pid, sched_switch_class))
    triggers.append(sched_switch_out_trigger(ftrace, pid, sched_switch_class))
    return triggers

def sched_switch_in_trigger(ftrace, pid, sched_switch_class):
    """
    :param ftrace: A ftrace object with a sched_switch
        event
    :type ftrace: :mod:`trappy.ftrace.FTrace`

    :param pid: The PID of the associated process
    :type pid: int

    :param sched_switch_class: The SchedSwitch event class
    :type sched_switch_class: :mod:`trappy.base.Base`

    :return: :mod:`trappy.stats.Trigger.Trigger` on
        the SchedSwitch: IN for the given PID
    """

    task_in = {}
    task_in[NEXT_PID_FIELD] = pid

    return Trigger(ftrace,
                   sched_switch_class,              # trappy Event Class
                   task_in,                         # Filter Dictionary
                   SCHED_SWITCH_IN,                 # Trigger Value
                   CPU_FIELD)                       # Primary Pivot

def sched_switch_out_trigger(ftrace, pid, sched_switch_class):
    """
    :param ftrace: A ftrace object with a sched_switch
        event
    :type ftrace: :mod:`trappy.ftrace.FTrace`

    :param pid: The PID of the associated process
    :type pid: int

    :param sched_switch_class: The SchedSwitch event class
    :type sched_switch_class: :mod:`trappy.base.Base`

    :return: :mod:`trappy.stats.Trigger.Trigger` on
        the SchedSwitch: OUT for the given PID
    """

    task_out = {}
    task_out[PREV_PID_FIELD] = pid

    return Trigger(ftrace,
                   sched_switch_class,              # trappy Event Class
                   task_out,                        # Filter Dictionary
                   SCHED_SWITCH_OUT,                # Trigger Value
                   CPU_FIELD)                       # Primary Pivot


def trace_event(series, window=None):
    """
    :func:`aggfunc` to be used for plotting
    the process residency data using
    :mod:`trappy.plotter.EventPlot`

    :param series: Input Time Series data
    :type series: :mod:`pandas.Series`

    :param window: A tuple indicating a time window
    :type window: tuple

    :return: A list of events
        of the type:
        ::

            [
                [start_time_1, stop_time_1],
                [start_time_2, stop_time_2],
                #
                #
                [start_time_N, stop_time_N],
            ]
    """
    rects = []
    series = select_window(series, window)
    series = sanitize_asymmetry(series, window)

    s_in = series[series == SCHED_SWITCH_IN]
    s_out = series[series == SCHED_SWITCH_OUT]

    if not len(s_in):
        return rects

    if len(s_in) != len(s_out):
        raise RuntimeError(
            "Unexpected Lengths: s_in={}, s_out={}".format(
                len(s_in),
                len(s_out)))

    return np.column_stack((s_in.index.values, s_out.index.values))
