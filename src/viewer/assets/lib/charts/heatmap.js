import {
    createAxisLabelFormatter
} from './util/units.js';
import {
    formatDateTime
} from './util/utils.js';
import {
    getBaseOption,
    getNoDataOption,
} from './base.js';

/**
 * Configures a heatmap chart based on Chart.spec
 * @param {import('./chart.js').Chart} chart - the chart to configure
 */
export function configureHeatmap(chart) {
    const {
        time_data: timeData,
        data,
        min_value: minValue,
        max_value: maxValue,
        opts
    } = chart.spec;

    if (!data || data.length < 1 || !timeData || timeData.length === 0) {
        chart.echart.setOption(getNoDataOption(opts.title));
        return;
    }

    // Pass chartsState for global time range and interval
    const baseOption = getBaseOption(opts.title, chart.chartsState);

    // Extract all unique Y indices
    const yIndices = new Set();
    data.forEach(item => {
        yIndices.add(item[1]);
    });

    const yIds = Array.from(yIndices).sort((a, b) => a - b);

    const maxYId = yIds.length > 0 ? Math.max(...yIds) : 0;
    const continuousYIds = Array.from({
        length: maxYId + 1
    }, (_, i) => i);

    // Transform data into a 2d matrix
    const xCount = timeData.length;
    const yCount = continuousYIds.length;
    const dataMatrix = new Array(yCount).fill(null).map(() => new Array(xCount).fill(null));
    for (let i = 0; i < data.length; i++) {
        const [timeIndex, y, value] = data[i];
        dataMatrix[y][timeIndex] = value;
    }

    const processedData = [];
    for (let i = 0; i < data.length; i++) {
        const [timeIndex, y, value] = data[i];
        if (timeIndex >= 0 && timeIndex < timeData.length) {
            processedData.push([timeData[timeIndex] * 1000, y, timeIndex, null, value]);
        }
    }

    const MAX_DATA_POINT_DISPLAY = 50000;
    chart.downsampleCache = [];
    chart.downsampleCache.push({ factor: 1, data: processedData, renderItem: createRenderItemFunc(timeData, 1) });
    const originalRatioOfDataPointsToMax = xCount * yCount / MAX_DATA_POINT_DISPLAY;

    if (originalRatioOfDataPointsToMax > 1) {
        const factor = Math.ceil(originalRatioOfDataPointsToMax);
        const downsampledData = downsample(dataMatrix, factor);
        const downsampledXCount = downsampledData[0].length;
        const processedDownsampledData = [];
        for (let y = 0; y < yCount; y++) {
            for (let x = 0; x < downsampledXCount; x++) {
                const minAndMax = downsampledData[y][x];
                if (minAndMax !== null) {
                    processedDownsampledData.push([timeData[x * factor] * 1000, y, x * factor, minAndMax[0], minAndMax[1]]);
                }
            }
        }
        chart.downsampleCache.push({ factor, data: processedDownsampledData, renderItem: createRenderItemFunc(timeData, factor) });
    }

    const Y_MAX_LABELS = 16;
    const yLabelMultiple = Math.pow(2, Math.ceil(Math.log2(Math.ceil(yCount / Y_MAX_LABELS))));
    const yAxisLabelInterval = yLabelMultiple - 1;
    const Y_MAX_TICKS_PER_LABEL = 4;
    const yTickMultiple = Math.ceil(yLabelMultiple / Y_MAX_TICKS_PER_LABEL);
    const yAxisTickInterval = yTickMultiple - 1;

    const format = opts.format || {};
    const unitSystem = format.unit_system;
    const yAxisLabel = format.y_axis_label || format.axis_label;
    const valueLabel = format.value_label;

    let tooltipFormatter = function (params) {
        if (params.data === undefined) {
            return '';
        }
        const [time, idx, timeIndex, minVal, value] = params.data;

        const formattedTime = formatDateTime(time);

        let label, formattedValue, formattedMinValue;
        if (unitSystem) {
            const formatter = createAxisLabelFormatter(unitSystem);
            label = valueLabel ? `<span style="margin-left: 10px;">${valueLabel}: </span>` : '';
            formattedValue = formatter(value);
            formattedMinValue = minVal === null ? '' : formatter(minVal);
        } else {
            label = '';
            formattedValue = value.toFixed(6);
            formattedMinValue = minVal === null ? '' : minVal.toFixed(6);
        }
        const valueString = minVal === null ? formattedValue : `${formattedMinValue} - ${formattedValue}`;
        return `<div>
                    <div>
                        ${formattedTime}
                    </div>
                    <div style="margin-top: 5px;">
                        <span style="background-color: #aaaaff33; padding: 2px 5px; border-radius: 4px; font-size: .85em;">
                            Index: ${idx}
                        </span>
                        ${label}
                        <span style="font-weight: bold; float: right; margin-left: 20px;">
                            ${valueString}
                        </span>
                    </div>
                </div>`;
    };

    const yAxis = {
        type: 'category',
        name: yAxisLabel || 'Index',
        nameLocation: 'middle',
        nameGap: 40,
        nameTextStyle: {
            color: '#E0E0E0',
            fontSize: 14,
            padding: [0, 0, 0, 20]
        },
        data: continuousYIds,
        axisLabel: {
            interval: yAxisLabelInterval,
            color: '#ABABAB'
        },
        axisTick: {
            interval: yAxisTickInterval,
        }
    };

    const option = {
        ...baseOption,
        yAxis,
        hoverLayerThreshold: 0,
        tooltip: {
            ...baseOption.tooltip,
            trigger: 'item',
            axisPointer: {
                type: 'line',
                animation: false,
                lineStyle: {
                    color: '#E0E0E0',
                },
                label: {
                    backgroundColor: '#505765'
                }
            },
            position: 'top',
            formatter: tooltipFormatter,
            textStyle: {
                color: '#E0E0E0'
            },
            backgroundColor: 'rgba(50, 50, 50, 0.8)',
            borderColor: 'rgba(70, 70, 70, 0.8)',
        },
        visualMap: {
            type: 'continuous',
            min: minValue,
            max: maxValue,
            calculable: false,
            show: false,
            inRange: {
                color: [
                    '#440154', '#481a6c', '#472f7d', '#414487', '#39568c',
                    '#31688e', '#2a788e', '#23888e', '#1f988b', '#22a884',
                    '#35b779', '#54c568', '#7ad151', '#a5db36', '#d2e21b', '#fde725'
                ]
            }
        },
        series: [{
            name: chart.spec.opts.title,
            type: 'custom',
            renderItem: chart.downsampleCache[chart.downsampleCache.length - 1].renderItem,
            clip: true,
            data: chart.downsampleCache[chart.downsampleCache.length - 1].data,
            emphasis: {
                itemStyle: {
                    shadowBlur: 10,
                    shadowColor: 'rgba(0, 0, 0, 0.5)'
                }
            },
            progressive: 8000,
            progressiveThreshold: 3000,
            animation: false
        }]
    };

    chart.echart.setOption(option);

    chart.echart.on('datazoom', (event) => {
        const zoomLevel = event.batch ? event.batch[0] : event;
        const factor = zoomLevelToFactor(zoomLevel, originalRatioOfDataPointsToMax, 1000 * (timeData[timeData.length - 1] - timeData[0]));
        for (let i = 0; i < chart.downsampleCache.length; i++) {
            const downsampleCacheItem = chart.downsampleCache[i];
            if (downsampleCacheItem.factor >= factor) {
                const data = downsampleCacheItem.data;
                const renderItem = downsampleCacheItem.renderItem;
                if (chart.echart.getOption().series[0].data.length !== data.length) {
                    chart.echart.setOption({
                        series: [{
                            data: data,
                            renderItem: renderItem
                        }]
                    });
                }
                break;
            }
        }
    });
}

const downsample = (dataMatrix, factor) => {
    const yCount = dataMatrix.length;
    const xCount = dataMatrix[0].length;
    const downsampledXCount = Math.ceil(xCount / factor);
    const downsampledDataMatrix = new Array(yCount).fill(null).map(() => new Array(downsampledXCount).fill(null));
    for (let y = 0; y < yCount; y++) {
        for (let x = 0; x < Math.ceil(xCount / factor); x++) {
            let max = null;
            let min = null;
            for (let origX = x * factor; origX < (x + 1) * factor && origX < xCount; origX++) {
                if (dataMatrix[y][origX] !== null) {
                    if (max === null) {
                        max = dataMatrix[y][origX];
                        min = dataMatrix[y][origX];
                    } else {
                        max = Math.max(max, dataMatrix[y][origX]);
                        min = Math.min(min, dataMatrix[y][origX]);
                    }
                }
            }
            if (max !== null) {
                downsampledDataMatrix[y][x] = [min, max];
            }
        }
    }
    return downsampledDataMatrix;
}

const zoomLevelToFactor = (zoomLevel, originalRatioOfDataPointsToMax, originalXDifference) => {
    const { start, end, startValue, endValue } = zoomLevel;
    if (start !== undefined && end !== undefined) {
        const fraction = (end - start) / 100;
        if (fraction <= 0) {
            return 1;
        }
        return Math.ceil(originalRatioOfDataPointsToMax * fraction);
    } else if (startValue !== undefined && endValue !== undefined) {
        const fraction = (endValue - startValue) / originalXDifference;
        if (fraction <= 0) {
            return 1;
        }
        return Math.ceil(originalRatioOfDataPointsToMax * fraction);
    }
    return Math.ceil(originalRatioOfDataPointsToMax);
}

const createRenderItemFunc = (timeData, factor) => {
    return function (params, api) {
        const x = api.value(0);
        const y = api.value(1);
        const timeIndex = api.value(2);
        const nextX = timeData[timeIndex + factor] * 1000 || Number.MAX_VALUE;
        const start = api.coord([x, y]);
        const end = api.coord([nextX, y]);
        const width = end[0] - start[0] + 1;
        const height = api.size([0, 1])[1];
        return (
            {
                type: 'rect',
                transition: [],
                shape: {
                    x: start[0],
                    y: start[1] - height / 2,
                    width: width,
                    height: height
                },
                style: {
                    fill: api.style().fill
                }
            }
        );
    };
}
