// histogram_heatmap.js - Histogram heatmap chart for latency distributions

import {
    formatDateTime
} from './util/utils.js';
import {
    getBaseOption,
    getNoDataOption,
} from './base.js';

/**
 * Format a latency bucket boundary for display
 * @param {number} nanoseconds - The bucket boundary in nanoseconds
 * @returns {string} Formatted latency string
 */
function formatLatencyBucket(nanoseconds) {
    if (nanoseconds < 1000) {
        return `${nanoseconds}ns`;
    } else if (nanoseconds < 1000000) {
        return `${(nanoseconds / 1000).toFixed(1)}us`;
    } else if (nanoseconds < 1000000000) {
        return `${(nanoseconds / 1000000).toFixed(1)}ms`;
    } else {
        return `${(nanoseconds / 1000000000).toFixed(2)}s`;
    }
}

/**
 * Inferno color scale interpolation
 * @param {number} t - Value from 0 to 1
 * @returns {string} RGB color string
 */
function infernoColor(t) {
    // Inferno colormap approximation - starts from black
    const colors = [
        [0, 0, 0],       // Black for zero/empty
        [20, 11, 54],    // Very dark purple
        [63, 17, 100],
        [113, 31, 107],
        [159, 42, 99],
        [212, 72, 66],
        [245, 125, 21],
        [250, 193, 39],
        [252, 255, 164]
    ];

    const idx = t * (colors.length - 1);
    const i = Math.floor(idx);
    const f = idx - i;

    if (i >= colors.length - 1) {
        return `rgb(${colors[colors.length - 1].join(',')})`;
    }

    const c0 = colors[i];
    const c1 = colors[i + 1];
    const r = Math.round(c0[0] + f * (c1[0] - c0[0]));
    const g = Math.round(c0[1] + f * (c1[1] - c0[1]));
    const b = Math.round(c0[2] + f * (c1[2] - c0[2]));

    return `rgb(${r},${g},${b})`;
}

/**
 * Configures the Chart for histogram heatmap visualization
 * Uses a log-scale Y-axis with custom series for proper bucket sizing
 * @param {import('./chart.js').Chart} chart - the chart to configure
 */
export function configureHistogramHeatmap(chart) {
    const {
        time_data: timeData,
        bucket_bounds: bucketBounds,
        data,
        min_value: minValue,
        max_value: maxValue,
        opts
    } = chart.spec;

    if (!data || data.length === 0 || !timeData || timeData.length === 0) {
        chart.echart.setOption(getNoDataOption(opts.title));
        return;
    }

    // Pass chartsState for global time range and interval
    const baseOption = getBaseOption(opts.title, chart.chartsState);

    // Find the range of buckets that actually have data
    let minBucketIdx = Infinity;
    let maxBucketIdx = -Infinity;
    for (const [_, bucketIdx, count] of data) {
        if (count > 0) {
            minBucketIdx = Math.min(minBucketIdx, bucketIdx);
            maxBucketIdx = Math.max(maxBucketIdx, bucketIdx);
        }
    }

    // Add some padding around the range
    minBucketIdx = Math.max(0, minBucketIdx - 1);
    maxBucketIdx = Math.min(bucketBounds.length - 1, maxBucketIdx + 1);

    // Get the visible bucket bounds for log scale
    // bucketBounds[i] is the END of bucket i, so:
    // - min Y value is the START of minBucketIdx (which is bucketBounds[minBucketIdx-1] or 1)
    // - max Y value is the END of maxBucketIdx (which is bucketBounds[maxBucketIdx])
    const minBucketValue = minBucketIdx > 0 ? Math.max(1, bucketBounds[minBucketIdx - 1]) : 1;
    const maxBucketValue = bucketBounds[maxBucketIdx];

    // Calculate the time interval for cell width (in milliseconds)
    const timeIntervalMs = timeData.length > 1
        ? (timeData[1] - timeData[0]) * 1000
        : 1000;

    // Create a lookup map for data points: key = "timeIdx-bucketIdx", value = count
    const dataMap = new Map();
    for (const [timeIdx, bucketIdx, count] of data) {
        if (bucketIdx >= minBucketIdx && bucketIdx <= maxBucketIdx) {
            dataMap.set(`${timeIdx}-${bucketIdx}`, count);
        }
    }

    // bucketBounds contains the END value of each bucket
    // So bucket N spans from bucketBounds[N-1] (or 0) to bucketBounds[N]
    //
    // Only generate data for cells with non-zero counts for performance.
    // Each data point: [timestamp_ms, bucketLowerBound, bucketUpperBound, normalizedCount]
    //
    // We normalize the count by the log-width of the bucket to get a density estimate.
    // This prevents wider buckets (in log space) from appearing brighter just because
    // they cover a larger range of values.
    const cellsData = [];
    let normalizedMin = Infinity;
    let normalizedMax = -Infinity;

    for (const [timeIdx, bucketIdx, count] of data) {
        if (count <= 0 || bucketIdx < minBucketIdx || bucketIdx > maxBucketIdx) {
            continue;
        }

        const timestampMs = timeData[timeIdx] * 1000;
        const lowerBound = bucketIdx > 0 ? Math.max(1, bucketBounds[bucketIdx - 1]) : 1;
        const upperBound = bucketBounds[bucketIdx];

        // Normalize count by bucket width in log space to get density
        const logWidth = Math.log(upperBound) - Math.log(lowerBound);
        const normalizedCount = logWidth > 0 ? count / logWidth : count;

        cellsData.push([timestampMs, lowerBound, upperBound, normalizedCount]);

        normalizedMin = Math.min(normalizedMin, normalizedCount);
        normalizedMax = Math.max(normalizedMax, normalizedCount);
    }

    if (normalizedMin === Infinity) normalizedMin = 0;
    if (normalizedMax === -Infinity) normalizedMax = 1;

    // Use log scale for color mapping to better show distribution
    const logMin = Math.log1p(normalizedMin);
    const logMax = Math.log1p(normalizedMax);
    const logRange = logMax - logMin || 1;

    // Configure tooltip
    const tooltipFormatter = function (params) {
        if (!params.data) {
            return '';
        }
        const [timestampMs, lowerBound, upperBound, count] = params.data;
        const formattedTime = formatDateTime(timestampMs);
        const bucketLabel = formatLatencyBucket(lowerBound);

        return `<div>
            <div>${formattedTime}</div>
            <div style="margin-top: 5px;">
                <span style="background-color: rgba(59, 130, 246, 0.15); padding: 2px 6px; border-radius: 4px; font-size: .85em; color: #3b82f6;">
                    Latency: ${bucketLabel}
                </span>
                <span style="font-weight: 600; float: right; margin-left: 20px;">
                    Count: ${Math.round(count)}
                </span>
            </div>
        </div>`;
    };

    // Custom render function for heatmap cells with proper log-scale sizing
    const renderItem = function (params, api) {
        const timestampMs = api.value(0);
        const lowerBound = api.value(1);
        const upperBound = api.value(2);
        const count = api.value(3);

        // Get the coordinate rect info for clipping
        const coordSys = params.coordSys;
        const gridX = coordSys.x;
        const gridY = coordSys.y;
        const gridWidth = coordSys.width;
        const gridHeight = coordSys.height;

        // Cell spans from (timestamp - interval) to timestamp
        // This matches the line chart's step: 'start' behavior where data at time T
        // represents the interval ending at T
        const cellStartMs = timestampMs - timeIntervalMs;
        const cellEndMs = timestampMs;

        // Get pixel coordinates for cell boundaries
        const xStartCoord = api.coord([cellStartMs, lowerBound]);
        const xEndCoord = api.coord([cellEndMs, upperBound]);
        if (!xStartCoord || !xEndCoord) return;

        // Add small overlap to prevent gaps from floating-point precision
        const overlap = 1;
        let x = xStartCoord[0];
        let y = xEndCoord[1] - overlap;  // Upper bound is higher on screen (lower y pixel)
        let width = xEndCoord[0] - xStartCoord[0] + overlap;
        let height = xStartCoord[1] - xEndCoord[1] + overlap * 2;

        // Skip cells that are too small or have zero/negative dimensions
        if (width <= 0 || height <= 0) return;

        // Clip to grid boundaries
        if (x < gridX) {
            width -= (gridX - x);
            x = gridX;
        }
        if (x + width > gridX + gridWidth) {
            width = gridX + gridWidth - x;
        }
        if (y < gridY) {
            height -= (gridY - y);
            y = gridY;
        }
        if (y + height > gridY + gridHeight) {
            height = gridY + gridHeight - y;
        }
        if (width <= 0 || height <= 0) return;

        // Map count to color using log scale (0 = black)
        const logCount = Math.log1p(count);
        const normalizedValue = Math.min(1, Math.max(0, (logCount - logMin) / logRange));
        const color = infernoColor(normalizedValue);

        return {
            type: 'rect',
            shape: {
                x: x,
                y: y,
                width: width,
                height: height
            },
            style: {
                fill: color,
                stroke: null,
                lineWidth: 0
            }
        };
    };

    // Calculate minimum zoom span (5x sample interval as percentage of total duration)
    const sampleInterval = timeData.length > 1 ? timeData[1] - timeData[0] : 1;
    const totalDuration = timeData[timeData.length - 1] - timeData[0];
    const minZoomSpan = Math.max(0.1, (sampleInterval * 5 / totalDuration) * 100);

    // Get current zoom state from chartsState
    const zoomLevel = chart.chartsState?.zoomLevel;

    const option = {
        ...baseOption,
        grid: {
            left: '80',
            right: '24',
            top: '50',
            bottom: '35',
            containLabel: false,
        },
        // Add dataZoom component with minSpan to enforce minimum zoom level
        dataZoom: [{
            type: 'inside',
            xAxisIndex: 0,
            minSpan: minZoomSpan,
            filterMode: 'none',
        }, {
            type: 'slider',
            show: false,
            xAxisIndex: 0,
            minSpan: minZoomSpan,
            filterMode: 'none',
        }],
        xAxis: {
            ...baseOption.xAxis,
            // Override min/max if not set by baseOption (use global time range)
            min: baseOption.xAxis.min !== 'dataMin' ? baseOption.xAxis.min : 'dataMin',
            max: baseOption.xAxis.max !== 'dataMax' ? baseOption.xAxis.max : 'dataMax',
        },
        yAxis: {
            type: 'log',
            min: minBucketValue,
            max: maxBucketValue,
            axisLine: { show: false },
            axisTick: { show: false },
            axisLabel: {
                color: '#6b7280',
                fontSize: 10,
                formatter: (value) => formatLatencyBucket(value),
            },
            splitLine: {
                show: false,
            },
        },
        tooltip: {
            ...baseOption.tooltip,
            trigger: 'item',
            position: 'top',
            formatter: tooltipFormatter,
        },
        series: [{
            name: opts.title,
            type: 'custom',
            renderItem: renderItem,
            encode: {
                x: 0,
                y: [1, 2],
            },
            data: cellsData,
            clip: true,  // Clip rendering to the grid area
            animation: false,
            // Force all data to render (no progressive rendering)
            progressive: 0,
            progressiveThreshold: 0,
        }]
    };

    // Use notMerge: true to clear any previous chart configuration (e.g., scatter series)
    chart.echart.setOption(option, { notMerge: true });

    // Re-enable drag-to-zoom after clearing the chart
    chart.echart.dispatchAction({
        type: 'takeGlobalCursor',
        key: 'dataZoomSelect',
        dataZoomSelectActive: true,
    });

    // Apply zoom state if present
    // Use startValue/endValue for value-based zoom, or start/end for percentage-based
    const hasZoom = zoomLevel && (
        (zoomLevel.start !== undefined && zoomLevel.start !== 0) ||
        (zoomLevel.end !== undefined && zoomLevel.end !== 100) ||
        zoomLevel.startValue !== undefined ||
        zoomLevel.endValue !== undefined
    );

    if (hasZoom) {
        // Wait for next frame to ensure chart is fully rendered
        requestAnimationFrame(() => {
            requestAnimationFrame(() => {
                if (chart.echart) {
                    chart.echart.dispatchAction({
                        type: 'dataZoom',
                        dataZoomIndex: 0,
                        start: zoomLevel.start,
                        end: zoomLevel.end,
                        startValue: zoomLevel.startValue,
                        endValue: zoomLevel.endValue,
                    });
                }
            });
        });
    }
}
