import {
    createAxisLabelFormatter,
} from './util/units.js';
import {
    getBaseOption,
    getBaseYAxisOption,
    getTooltipFormatter,
    getNoDataOption,
} from './base.js';

/**
 * Configures a line chart based on Chart.spec
 * @param {import('./chart.js').Chart} chart - the chart to configure
 */
export function configureLineChart(chart) {
    const {
        data,
        opts
    } = chart.spec;

    if (
        !data ||
        data.length < 2 ||
        !data[0] ||
        !data[1] ||
        data[0].length === 0
    ) {
        chart.echart.setOption(getNoDataOption(opts.title));
        return;
    }

    // Pass chartsState for global time range and interval
    const baseOption = getBaseOption(opts.title, chart.chartsState);

    const [timeData, valueData] = data;

    const zippedData = timeData.map((t, i) => [t * 1000, valueData[i]]);

    const format = opts.format || {};
    const unitSystem = format.unit_system;
    const logScale = format.log_scale;
    let minValue = format.min;
    let maxValue = format.max;

    // For percentage charts, ensure minimum 1% Y-axis span
    if (unitSystem === 'percentage' && (minValue === undefined || minValue === null) && (maxValue === undefined || maxValue === null) && valueData.length > 0) {
        const validData = valueData.filter(v => v !== null && v !== undefined && !isNaN(v));
        if (validData.length > 0) {
            const dataMin = Math.min(...validData);
            const dataMax = Math.max(...validData);
            const dataRange = dataMax - dataMin;
            const minSpan = 0.01; // 1% minimum span

            if (dataRange < minSpan) {
                // Center the span around the data
                const dataMid = (dataMin + dataMax) / 2;
                minValue = Math.max(0, dataMid - minSpan / 2);
                maxValue = Math.min(1, dataMid + minSpan / 2);

                // Adjust if we hit boundaries
                if (minValue === 0) {
                    maxValue = Math.min(1, minSpan);
                } else if (maxValue === 1) {
                    minValue = Math.max(0, 1 - minSpan);
                }
            }
        }
    }

    const option = {
        ...baseOption,
        yAxis: getBaseYAxisOption(logScale, minValue, maxValue, unitSystem),
        tooltip: {
            ...baseOption.tooltip,
            formatter: getTooltipFormatter(unitSystem ?
                createAxisLabelFormatter(unitSystem) :
                val => val),
        },
        series: [{
            data: zippedData,
            type: 'line',
            name: opts.title,
            showSymbol: false,
            emphasis: {
                focus: 'series'
            },
            step: 'start',
            lineStyle: {
                width: 2,
                color: '#00d4ff'
            },
            itemStyle: {
                color: '#00d4ff'
            },
            areaStyle: {
                color: {
                    type: 'linear',
                    x: 0,
                    y: 0,
                    x2: 0,
                    y2: 1,
                    colorStops: [{
                        offset: 0,
                        color: 'rgba(0, 212, 255, 0.3)'
                    }, {
                        offset: 1,
                        color: 'rgba(0, 212, 255, 0.05)'
                    }]
                }
            },
            animationDuration: 0,
        }]
    };

    chart.echart.setOption(option);
}
