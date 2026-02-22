import {
    createAxisLabelFormatter,
} from './util/units.js';
import {
    getBaseOption,
    getBaseYAxisOption,
    getTooltipFormatter,
    getNoDataOption,
} from './base.js';
import globalColorMapper from './util/colormap.js';

/**
 * Configures a multi-series line chart based on Chart.spec
 * @param {import('./chart.js').Chart} chart - the chart to configure
 */
export function configureMultiSeriesChart(chart) {
    const {
        data,
        opts,
    } = chart.spec;

    if (!data || data.length < 2 || !data[0] || data[0].length === 0) {
        chart.echart.setOption(getNoDataOption(opts.title));
        return;
    }

    // Pass chartsState for global time range and interval
    const baseOption = getBaseOption(opts.title, chart.chartsState);

    // For multi-series charts, the first row contains timestamps, subsequent rows are series data
    const timeData = data[0];
    const lineCount = data.length - 1;

    let seriesNames = chart.spec.series_names;
    if (!seriesNames || seriesNames.length !== lineCount) {
        console.log("series_names is missing or wrong length", seriesNames);
        seriesNames = Array.from(Array(lineCount).keys()).map(i => `Series ${i + 1}`);
    }

    const format = opts.format || {};
    const unitSystem = format.unit_system;
    const logScale = format.log_scale;
    const minValue = format.min;
    const maxValue = format.max;

    // Create series configurations for each data series
    const series = [];

    // Get colors for series
    const seriesColors = seriesNames.map((name) => {
        const color = globalColorMapper.getSelectedCgroupColor(name);
        return (
            color ||
            globalColorMapper.getColorByIndex(seriesNames.indexOf(name))
        );
    });

    for (let i = 1; i < data.length; i++) {
        const name = seriesNames[i - 1];
        const isOtherCategory = name === "Other";
        const color = seriesColors[i - 1];

        const zippedData = timeData.map((t, j) => [t * 1000, data[i][j]]);

        series.push({
            name: name,
            type: 'line',
            data: zippedData,
            itemStyle: {
                color,
            },
            step: 'start',
            lineStyle: {
                color,
                width: 2,
            },
            showSymbol: isOtherCategory,
            symbolSize: isOtherCategory ? 4 : 0,
            z: isOtherCategory ? 1 : 2,
            emphasis: {
                focus: 'series'
            },
            animationDuration: 0
        });
    }

    // Ensure "Other" category is the last in the series array
    const otherIndex = series.findIndex(s => s.name === "Other");
    if (otherIndex !== -1 && otherIndex !== series.length - 1) {
        const otherSeries = series.splice(otherIndex, 1)[0];
        series.push(otherSeries);
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
        series: series,
        color: seriesColors,
    };

    chart.echart.setOption(option);
}
