import {
    configureLineChart
} from './line.js';
import {
    configureScatterChart
} from './scatter.js';
import {
    configureHeatmap
} from './heatmap.js';
import {
    configureHistogramHeatmap
} from './histogram_heatmap.js';
import {
    configureMultiSeriesChart
} from './multi.js';
import globalColorMapper from './util/colormap.js';


export class ChartsState {
    // Zoom state for synchronization across charts
    zoomLevel = null;
    // All `Chart` instances, mapped by id
    charts = new Map();
    // Global color mapper
    colorMapper = globalColorMapper;
    // Sampling interval in seconds (for minimum zoom calculation)
    interval = null;
    // Total time range in seconds
    timeRange = null;
    // Global min/max time in seconds (for consistent x-axis across all charts)
    minTime = null;
    maxTime = null;

    // Set the interval and time range from view data
    setTimeInfo(interval, minTime, maxTime) {
        this.interval = interval;
        this.timeRange = maxTime - minTime;
        this.minTime = minTime;
        this.maxTime = maxTime;
    }

    // Calculate the minimum zoom percentage (5x the sampling interval)
    getMinZoomPercent() {
        if (!this.interval || !this.timeRange || this.timeRange <= 0) {
            return 0; // No limit if we don't have the info
        }
        const minSpan = this.interval * 5; // 5x the sampling interval
        const minPercent = (minSpan / this.timeRange) * 100;
        return Math.min(minPercent, 100); // Cap at 100%
    }

    // Resets charts state
    clear() {
        this.zoomLevel = null;
        this.charts.clear();
    }
}

// Chart component - uses echarts to render a chart
export class Chart {
    constructor(vnode) {
        this.domNode = null;
        this.chartId = vnode.attrs.spec.opts.id;
        this.spec = vnode.attrs.spec;
        this.chartsState = vnode.attrs.chartsState;
        this.resizeHandler = null;
        this.observer = null;
        this.echart = null;
    }

    oncreate(vnode) {
        this.domNode = vnode.dom;

        // Set up the Intersection Observer to lazy load the chart
        const observer = new IntersectionObserver((entries) => {
            const hasIntersection = entries.some(entry => entry.isIntersecting);
            if (hasIntersection) {
                this.initEchart();
                observer.unobserve(this.domNode);
            }
        }, {
            root: null,
            rootMargin: '1px',
            threshold: 0.01
        });

        observer.observe(this.domNode);

        const resizeHandler = () => {
            if (this.echart) {
                this.echart.resize();
            }
        };
        window.addEventListener('resize', resizeHandler);
        this.resizeHandler = resizeHandler;
        this.observer = observer;
    }

    onupdate(vnode) {
        const oldSpec = this.spec;
        this.spec = vnode.attrs.spec;

        // Check if chart style changed (e.g., scatter -> histogram_heatmap)
        const styleChanged = oldSpec.opts?.style !== this.spec.opts?.style;

        if (this.echart && (oldSpec.data !== this.spec.data || styleChanged)) {
            // Reconfigure the chart (zoom state will be included in configuration)
            this.configureChartByType();

            // Re-enable drag-to-zoom after reconfiguration
            this.echart.dispatchAction({
                type: 'takeGlobalCursor',
                key: 'dataZoomSelect',
                dataZoomSelectActive: true,
            });
        }
    }

    onremove() {
        if (this.observer) {
            this.observer.disconnect();
        }

        if (this.echart) {
            window.removeEventListener('resize', this.resizeHandler);
            this.echart.dispose();
            this.echart = null;
        }
    }

    view() {
        return m('div.chart');
    }

    isInitialized() {
        return this.echart !== null;
    }

    dispatchAction(action) {
        if (this.echart) {
            this.echart.dispatchAction(action);
        }
    }

    reinitialize() {
        if (this.isInitialized()) {
            this.echart.dispose();
            this.echart = null;
            this.initEchart();
        }
    }

    initEchart() {
        if (this.echart) {
            return;
        }

        this.echart = echarts.init(this.domNode);

        if (window.location.search.includes('debug')) {
            const startTime = new Date();
            this.echart.on('finished', () => {
                this.echart.off('finished');
                console.log(`Chart ${this.chartId} rendered in ${new Date() - startTime}ms`);
            });
        }

        if (this.spec.data && this.spec.data.length > 0) {
            if (this.spec.data[0] && Array.isArray(this.spec.data[0])) {
                this.echart.originalTimeData = this.spec.data[0];
            }
        } else if (this.spec.time_data) {
            this.echart.originalTimeData = this.spec.time_data;
        }

        this.chartsState.charts.set(this.chartId, this);

        this.configureChartByType();

        // Match existing zoom state
        if (this.chartsState.zoomLevel !== null) {
            if (this.chartsState.zoomLevel.start !== 0 || this.chartsState.zoomLevel.end !== 100) {
                this.echart.dispatchAction({
                    type: 'dataZoom',
                    start: this.chartsState.zoomLevel.start,
                    end: this.chartsState.zoomLevel.end,
                    startValue: this.chartsState.zoomLevel.startValue,
                    endValue: this.chartsState.zoomLevel.endValue,
                });
            }
        }

        this.echart.on('datazoom', (event) => {
            if (!event.batch) {
                return;
            }

            const details = event.batch[0];
            let { start, end, startValue, endValue } = details;

            // Enforce minimum zoom level (5x sampling interval)
            const minZoomPercent = this.chartsState.getMinZoomPercent();
            const currentSpan = end - start;

            if (currentSpan < minZoomPercent && minZoomPercent > 0) {
                // Zoom is too tight, clamp it
                const center = (start + end) / 2;
                start = Math.max(0, center - minZoomPercent / 2);
                end = Math.min(100, center + minZoomPercent / 2);

                // Adjust if we hit boundaries
                if (start === 0) {
                    end = Math.min(100, minZoomPercent);
                } else if (end === 100) {
                    start = Math.max(0, 100 - minZoomPercent);
                }

                // Clear the value-based zoom since we're using percentage
                startValue = undefined;
                endValue = undefined;
            }

            this.chartsState.zoomLevel = {
                start,
                end,
                startValue,
                endValue,
            };
            this.chartsState.charts.forEach(chart => {
                chart.dispatchAction({
                    type: 'dataZoom',
                    start,
                    end,
                    startValue,
                    endValue,
                });
            });
        });

        // Enable drag-to-zoom
        this.echart.dispatchAction({
            type: 'takeGlobalCursor',
            key: 'dataZoomSelect',
            dataZoomSelectActive: true,
        });

        // Double click to reset zoom
        this.echart.getZr().on('dblclick', () => {
            this.chartsState.zoomLevel = {
                start: 0,
                end: 100,
            };
            this.chartsState.charts.forEach(chart => {
                chart.dispatchAction({
                    type: 'dataZoom',
                    start: 0,
                    end: 100,
                });
            });
        })
    }

    configureChartByType() {
        const {
            opts
        } = this.spec;

        if (opts.style === 'line') {
            configureLineChart(this, this.spec, this.chartsState);
        } else if (opts.style === 'heatmap') {
            configureHeatmap(this, this.spec, this.chartsState);
        } else if (opts.style === 'histogram_heatmap') {
            configureHistogramHeatmap(this, this.spec, this.chartsState);
        } else if (opts.style === 'scatter') {
            configureScatterChart(this, this.spec, this.chartsState);
        } else if (opts.style === 'multi') {
            configureMultiSeriesChart(this, this.spec, this.chartsState);
        } else {
            throw new Error(`Unknown chart style: ${opts.style}`);
        }
    }
}
