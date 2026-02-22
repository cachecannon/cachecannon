import {
    createAxisLabelFormatter,
} from './util/units.js';
import { formatDateTime } from './util/utils.js';

// Color constants matching the CSS design tokens
export const COLORS = {
    // Foreground hierarchy
    fg: '#e6edf3',
    fgSecondary: '#8b949e',
    fgMuted: '#484f58',
    fgSubtle: '#30363d',

    // Accent colors
    accent: '#58a6ff',
    accentEmphasis: '#79c0ff',
    accentMuted: 'rgba(56, 139, 253, 0.4)',
    accentSubtle: 'rgba(56, 139, 253, 0.15)',
    accentGlow: 'rgba(56, 139, 253, 0.25)',

    // Backgrounds
    bgVoid: '#05080d',
    bgCard: '#0d1117',
    bgTertiary: '#161b22',
    bgElevated: '#1c2128',

    // Borders
    borderSubtle: 'rgba(48, 54, 61, 0.4)',
    borderDefault: 'rgba(48, 54, 61, 0.7)',

    // Grid lines - very subtle for clean charts
    gridLine: 'rgba(48, 54, 61, 0.5)',

    // Chart series colors - curated palette
    chartBlue: '#58a6ff',
    chartCyan: '#39d5ff',
    chartTeal: '#2dd4bf',
    chartGreen: '#3fb950',
    chartLime: '#a3e635',
    chartYellow: '#fbbf24',
    chartOrange: '#f97316',
    chartRed: '#f85149',
    chartPink: '#f472b6',
    chartPurple: '#a78bfa',
};

// Default chart color palette for multi-series charts
export const CHART_PALETTE = [
    COLORS.chartBlue,
    COLORS.chartCyan,
    COLORS.chartTeal,
    COLORS.chartGreen,
    COLORS.chartLime,
    COLORS.chartYellow,
    COLORS.chartOrange,
    COLORS.chartRed,
    COLORS.chartPink,
    COLORS.chartPurple,
];

/**
 * Creates a placeholder option for charts with no data
 * @param {string} title - The title of the chart
 * @returns {Object} ECharts option object for no-data placeholder
 */
export function getNoDataOption(title) {
    return {
        title: {
            text: title,
            left: '16',
            top: '12',
            textStyle: {
                color: COLORS.fg,
                fontSize: 13,
                fontWeight: 600,
                fontFamily: '"JetBrains Mono", "SF Mono", monospace',
            },
        },
        graphic: {
            type: 'text',
            left: 'center',
            top: 'middle',
            style: {
                text: 'No data available',
                fontSize: 12,
                fill: COLORS.fgMuted,
                font: 'normal 12px "Inter", -apple-system, sans-serif',
            },
        },
        xAxis: {
            show: false,
        },
        yAxis: {
            show: false,
        },
        grid: {
            left: '60',
            right: '24',
            top: '50',
            bottom: '35',
        },
    };
}

/**
 * Approximates echarts' built-in tooltip formatter, but with our own x axis formatting
 * (using formatDateTime) and our own value formatting (using valueFormatter).
 * @param {function} valueFormatter - A function from raw value to formatted value.
 */
export function getTooltipFormatter(valueFormatter) {
    return (paramsArray) => {
        // Sort the params array alphabetically by series name
        // Special handling: 'id' should come first in the sort if present
        const sortedParams = [...paramsArray].sort((a, b) => {
            const aName = a.seriesName;
            const bName = b.seriesName;

            // Extract id values if present (format is like "id=0, state=user")
            const aHasId = aName.startsWith('id=');
            const bHasId = bName.startsWith('id=');

            if (aHasId && bHasId) {
                // Both have ids, compare the full string naturally
                return aName.localeCompare(bName, undefined, { numeric: true });
            } else if (aHasId) {
                // a has id, b doesn't - a comes first
                return -1;
            } else if (bHasId) {
                // b has id, a doesn't - b comes first
                return 1;
            } else {
                // Neither has id, normal alphabetical sort
                return aName.localeCompare(bName, undefined, { numeric: true });
            }
        });

        const result =
            `<div style="font-family: 'Inter', -apple-system, sans-serif;">
                <div style="font-family: 'JetBrains Mono', monospace; font-size: 11px; color: ${COLORS.fgSecondary}; margin-bottom: 8px;">
                    ${formatDateTime(paramsArray[0].value[0])}
                </div>
                <div style="display: flex; flex-direction: column; gap: 4px;">
                    ${sortedParams.map(p => `<div style="display: flex; justify-content: space-between; align-items: center; gap: 16px;">
                        <span style="display: flex; align-items: center; gap: 6px;">
                            ${p.marker}
                            <span style="color: ${COLORS.fgSecondary}; font-size: 12px;">${p.seriesName}</span>
                        </span>
                        <span style="font-family: 'JetBrains Mono', monospace; font-weight: 600; font-size: 12px; color: ${COLORS.fg};">
                            ${valueFormatter(p.value[1])}
                        </span>
                    </div>`).join('')}
                </div>
            </div>`;

        return result;
    }
}

export function getBaseOption(title, chartsState = null) {
    // Calculate minimum zoom span (5x sampling interval in milliseconds)
    // If interval is provided in seconds, convert to ms
    const interval = chartsState ? chartsState.interval : null;
    const minValueSpan = interval ? interval * 5 * 1000 : undefined;

    // Use global time range if available (in milliseconds for echarts)
    const xMin = chartsState && chartsState.minTime != null ? chartsState.minTime * 1000 : 'dataMin';
    const xMax = chartsState && chartsState.maxTime != null ? chartsState.maxTime * 1000 : 'dataMax';

    return {
        grid: {
            left: '60',
            right: '24',
            top: '50',
            bottom: '35',
            containLabel: false,
        },
        xAxis: {
            type: 'time',
            min: xMin,
            max: xMax,
            splitNumber: 5,
            axisLine: { show: false },
            axisTick: { show: false },
            axisLabel: {
                color: COLORS.fgSecondary,
                fontSize: 10,
                fontFamily: '"JetBrains Mono", "SF Mono", monospace',
                formatter: {
                    year: '{yyyy}',
                    month: '{MMM}',
                    day: '{d}',
                    hour: '{HH}:{mm}',
                    minute: '{HH}:{mm}',
                    second: '{HH}:{mm}:{ss}',
                    millisecond: '{hh}:{mm}:{ss}.{SSS}',
                    none: '{hh}:{mm}:{ss}.{SSS}'
                }
            },
            splitLine: {
                show: true,
                lineStyle: {
                    color: COLORS.gridLine,
                    type: 'dashed',
                }
            },
        },
        tooltip: {
            trigger: 'axis',
            axisPointer: {
                type: 'line',
                snap: true,
                animation: false,
                lineStyle: {
                    color: COLORS.accent,
                    opacity: 0.5,
                },
                label: {
                    backgroundColor: COLORS.bgTertiary,
                    color: COLORS.fg,
                    fontFamily: '"JetBrains Mono", monospace',
                    fontSize: 10,
                }
            },
            textStyle: {
                color: COLORS.fg,
                fontFamily: '"Inter", -apple-system, sans-serif',
            },
            backgroundColor: 'rgba(13, 17, 23, 0.95)',
            borderColor: COLORS.borderDefault,
            borderRadius: 8,
            padding: [12, 16],
            extraCssText: 'box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);',
        },
        // Invisible toolbox for drag-to-zoom
        toolbox: {
            orient: 'vertical',
            itemSize: 13,
            top: 15,
            right: -6,
            feature: {
                dataZoom: {
                    yAxisIndex: 'none',
                    icon: {
                        zoom: 'path://',
                        back: 'path://',
                    },
                },
            },
        },
        // Hidden dataZoom component for programmatic zoom control
        dataZoom: [{
            type: 'inside',
            xAxisIndex: 0,
            filterMode: 'none',
            zoomOnMouseWheel: true,
            moveOnMouseMove: false,
            moveOnMouseWheel: false,
            minValueSpan: minValueSpan,
        }],
        title: {
            text: title,
            left: '16',
            top: '12',
            textStyle: {
                color: COLORS.fg,
                fontSize: 13,
                fontWeight: 600,
                fontFamily: '"JetBrains Mono", "SF Mono", monospace',
            }
        },
        textStyle: {
            color: COLORS.fg,
            fontFamily: '"Inter", -apple-system, sans-serif',
        },
        darkMode: true,
        backgroundColor: 'transparent'
    };
}

export function getBaseYAxisOption(logScale, minValue, maxValue, unitSystem) {
    return {
        type: logScale ? 'log' : 'value',
        logBase: 10,
        scale: true,
        min: minValue,
        max: maxValue,
        axisLine: { show: false },
        axisTick: { show: false },
        axisLabel: {
            color: COLORS.fgSecondary,
            fontSize: 10,
            fontFamily: '"JetBrains Mono", "SF Mono", monospace',
            margin: 12,
            formatter: unitSystem ?
                createAxisLabelFormatter(unitSystem) :
                function (value) {
                    if (logScale && Math.abs(value) >= 1000) {
                        return value.toExponential(0);
                    }
                    if (Math.abs(value) > 10000 || (Math.abs(value) > 0 && Math.abs(value) < 0.01)) {
                        return value.toExponential(1);
                    }
                    return value;
                }
        },
        splitLine: {
            lineStyle: {
                color: COLORS.gridLine,
                type: 'dashed',
            }
        }
    };
}
