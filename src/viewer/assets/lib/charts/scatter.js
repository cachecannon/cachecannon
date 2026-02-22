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
 * Configures a scatter chart based on Chart.spec
 * @param {import('./chart.js').Chart} chart - the chart to configure
 */
export function configureScatterChart(chart) {
    const {
        data,
        opts
    } = chart.spec;

    if (!data || data.length < 2 || !data[0] || data[0].length === 0) {
        chart.echart.setOption(getNoDataOption(opts.title));
        return;
    }

    // Pass chartsState for global time range and interval
    const baseOption = getBaseOption(opts.title, chart.chartsState);

    const format = opts.format || {};
    const unitSystem = format.unit_system;
    const logScale = format.log_scale;
    const minValue = format.min;
    const maxValue = format.max;

    // For percentile data, the format is [times, percentile1Values, percentile2Values, ...]
    const timeData = data[0];

    // Create series for each percentile
    const series = [];

    const percentileLabels = format.percentile_labels || ['p50', 'p90', 'p99', 'p99.9', 'p99.99'];

    // Color palette for percentile series (cyan-based gradient)
    const colors = ['#00d4ff', '#7ee787', '#ffa657', '#ff7b72', '#d2a8ff'];

    for (let i = 1; i < data.length; i++) {
        const percentileValues = data[i];
        const percentileData = [];

        for (let j = 0; j < timeData.length; j++) {
            if (percentileValues[j] !== undefined && !isNaN(percentileValues[j])) {
                percentileData.push([timeData[j] * 1000, percentileValues[j], j]);
            }
        }

        series.push({
            name: percentileLabels[i - 1] || `Percentile ${i}`,
            type: 'scatter',
            data: percentileData,
            symbolSize: 5,
            itemStyle: {
                color: colors[(i - 1) % colors.length]
            },
            emphasis: {
                focus: 'series',
                itemStyle: {
                    shadowBlur: 10,
                    shadowColor: 'rgba(255, 255, 255, 0.5)'
                }
            }
        });
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
        series: series
    };

    chart.echart.setOption(option);
}
