import { ChartsState, Chart } from './charts/chart.js';

// Global charts state for zoom synchronization
let chartsState = new ChartsState();

// Top navigation bar component
const TopNav = {
    view() {
        return m('div#topnav', [
            m('div.logo', 'CACHECANNON'),
        ]);
    },
};

// Sidebar component
const Sidebar = {
    view({ attrs }) {
        // Separate Query Explorer from other sections
        const regularSections = attrs.sections.filter(
            (s) => s.name !== 'Query Explorer',
        );
        const queryExplorer = attrs.sections.find(
            (s) => s.name === 'Query Explorer',
        );

        // Find Overview section
        const overviewSection = regularSections.find((s) => s.name === 'Overview');
        const dashboardSections = regularSections.filter((s) => s.name !== 'Overview');

        return m('div#sidebar', [
            // Overview section first (if exists)
            overviewSection && m(
                m.route.Link,
                {
                    class: attrs.activeSection === overviewSection ? 'selected' : '',
                    href: overviewSection.route,
                },
                overviewSection.name,
            ),

            // Dashboard sections label
            dashboardSections.length > 0 && m('div.sidebar-label', 'Dashboards'),

            // Dashboard sections
            dashboardSections.map((section) =>
                m(
                    m.route.Link,
                    {
                        class:
                            attrs.activeSection === section ? 'selected' : '',
                        href: section.route,
                    },
                    section.name,
                ),
            ),

            // Separator and Query Explorer if it exists
            queryExplorer && [
                m('div.sidebar-separator'),
                m(
                    m.route.Link,
                    {
                        class:
                            attrs.activeSection === queryExplorer
                                ? 'selected query-explorer-link'
                                : 'query-explorer-link',
                        href: queryExplorer.route,
                    },
                    [m('span.arrow', '→'), ' ', queryExplorer.name],
                ),
            ],
        ]);
    },
};

// Main component
const Main = {
    view({
        attrs: { activeSection, groups, sections, source, version, filename, interval },
    }) {
        // Format interval for display
        const intervalMs = interval ? Math.round(interval * 1000) : null;
        const intervalStr = intervalMs ? `${intervalMs}ms sampling` : '';

        return m('div', [
            m(TopNav),
            m('header', [
                m('div.file-info', [
                    m('div.filename', filename || 'benchmark.parquet'),
                    m('div.metadata', [
                        m('span', source || 'Cachecannon'),
                        version && m('span', version),
                        intervalStr && m('span', intervalStr),
                    ]),
                ]),
                m('div.header-actions', [
                    m('button', { onclick: () => chartsState.resetZoom() }, 'RESET ZOOM'),
                ]),
            ]),
            m('main', [
                m(Sidebar, {
                    activeSection,
                    sections,
                }),
                m(SectionContent, {
                    section: activeSection,
                    groups,
                }),
            ]),
        ]);
    },
};

// Query Explorer component for running custom PromQL queries
const QueryExplorer = {
    oninit(vnode) {
        vnode.state.query = '';
        vnode.state.result = null;
        vnode.state.error = null;
        vnode.state.loading = false;
        vnode.state.queryHistory = JSON.parse(
            localStorage.getItem('cachecannon_promql_history') || '[]',
        );
        vnode.state.queryChartsState = new ChartsState();

        vnode.state.formatValue = (value) => {
            if (Array.isArray(value) && value.length === 2) {
                const v = parseFloat(value[1]);
                if (Math.abs(v) < 0.01 && v !== 0) {
                    return v.toExponential(3);
                } else if (Math.abs(v) >= 1000000) {
                    return (v / 1000000).toFixed(2) + 'M';
                } else if (Math.abs(v) >= 1000) {
                    return (v / 1000).toFixed(2) + 'K';
                } else {
                    return v.toFixed(3);
                }
            }
            return String(value);
        };

        vnode.state.executeQuery = async () => {
            if (!vnode.state.query.trim()) return;

            vnode.state.loading = true;
            vnode.state.error = null;

            try {
                const metadataResponse = await m.request({
                    method: 'GET',
                    url: '/api/v1/metadata',
                    withCredentials: true,
                });

                if (metadataResponse.status !== 'success') {
                    throw new Error('Failed to get metadata');
                }

                const minTime = metadataResponse.data.minTime;
                const maxTime = metadataResponse.data.maxTime;
                const duration = maxTime - minTime;

                const windowDuration = Math.min(3600, duration);
                const start = Math.max(minTime, maxTime - windowDuration);
                const step = Math.max(1, Math.floor(windowDuration / 100));

                const url = `/api/v1/query_range?query=${encodeURIComponent(vnode.state.query)}&start=${start}&end=${maxTime}&step=${step}`;

                const response = await m.request({
                    method: 'GET',
                    url,
                    withCredentials: true,
                });

                vnode.state.result = response;
            } catch (error) {
                vnode.state.error = error.message || 'Query failed';
            }

            vnode.state.loading = false;

            if (
                !vnode.state.error &&
                vnode.state.result &&
                !vnode.state.queryHistory.includes(vnode.state.query)
            ) {
                vnode.state.queryHistory.unshift(vnode.state.query);
                vnode.state.queryHistory = vnode.state.queryHistory.slice(0, 20);
                localStorage.setItem(
                    'cachecannon_promql_history',
                    JSON.stringify(vnode.state.queryHistory),
                );
            }

            m.redraw();
        };
    },

    onremove(vnode) {
        if (vnode.state.queryChartsState) {
            vnode.state.queryChartsState.clear();
        }
    },

    view(vnode) {
        return m('div.query-explorer', [
            m('div.query-input-section', [
                m('h2', 'PromQL Query Explorer'),
                m('div.query-input-wrapper', [
                    m('textarea.query-input', {
                        placeholder:
                            'Enter a PromQL query (e.g., irate(requests_sent[5m]) or sum(irate(cache_hits[5m])))',
                        value: vnode.state.query,
                        oninput: (e) => (vnode.state.query = e.target.value),
                        onkeydown: (e) => {
                            if (e.key === 'Enter' && e.ctrlKey) {
                                vnode.state.executeQuery();
                            }
                        },
                    }),
                    m(
                        'button.execute-btn',
                        {
                            onclick: () => vnode.state.executeQuery(),
                            disabled: vnode.state.loading,
                        },
                        vnode.state.loading
                            ? 'Running...'
                            : 'Execute Query (Ctrl+Enter)',
                    ),
                ]),

                vnode.state.queryHistory.length > 0 &&
                    m('div.query-history', [
                        m('h3', 'Recent Queries'),
                        m(
                            'select.history-select',
                            {
                                onchange: (e) => {
                                    vnode.state.query = e.target.value;
                                },
                            },
                            [
                                m('option', { value: '' }, '-- Select from history --'),
                                vnode.state.queryHistory.map((q) =>
                                    m(
                                        'option',
                                        { value: q },
                                        q.length > 80 ? q.substring(0, 77) + '...' : q,
                                    ),
                                ),
                            ],
                        ),
                    ]),
            ]),

            vnode.state.error &&
                m('div.error-message', [
                    m('strong', 'Error: '),
                    vnode.state.error,
                ]),

            vnode.state.result &&
                m('div.query-result', [
                    m('h3', 'Result'),
                    vnode.state.result.status === 'success'
                        ? m('div.result-data', [
                              vnode.state.result.data &&
                              vnode.state.result.data.result &&
                              vnode.state.result.data.result.length > 0
                                  ? [
                                        (() => {
                                            const resultData =
                                                vnode.state.result.data.result;
                                            const hasMultipleSeries =
                                                resultData.length > 1;

                                            if (hasMultipleSeries) {
                                                const seriesNames = [];
                                                const allData = [];
                                                let timestamps = null;

                                                resultData.forEach((item, idx) => {
                                                    if (
                                                        item.values &&
                                                        Array.isArray(item.values)
                                                    ) {
                                                        let seriesName = 'Series ' + (idx + 1);
                                                        if (item.metric) {
                                                            const labels = [];
                                                            if (item.metric.id !== undefined) {
                                                                labels.push(`id=${item.metric.id}`);
                                                            }

                                                            const excludedLabels = ['__name__', 'id', 'metric', 'metric_type', 'unit'];
                                                            const otherLabels = Object.entries(item.metric)
                                                                .filter(([key, _]) => !excludedLabels.includes(key))
                                                                .sort((a, b) => a[0].localeCompare(b[0]))
                                                                .map(([key, value]) => `${key}=${value}`);

                                                            labels.push(...otherLabels);

                                                            if (labels.length > 0) {
                                                                seriesName = labels.join(', ');
                                                            }
                                                        }
                                                        seriesNames.push(seriesName);

                                                        if (!timestamps) {
                                                            timestamps = item.values.map(([ts, _]) => ts);
                                                            allData.push(timestamps);
                                                        }

                                                        const values = item.values.map(([_, val]) =>
                                                            parseFloat(val),
                                                        );
                                                        allData.push(values);
                                                    }
                                                });

                                                if (allData.length > 1) {
                                                    const chartKey = `query-chart-multi-${vnode.state.query}`;

                                                    const chartSpec = {
                                                        opts: {
                                                            id: chartKey,
                                                            title: 'Query Result',
                                                            style: 'multi',
                                                        },
                                                        data: allData,
                                                        series_names: seriesNames,
                                                    };

                                                    return m(
                                                        'div.query-chart',
                                                        { key: chartKey },
                                                        [
                                                            m(Chart, {
                                                                spec: chartSpec,
                                                                chartsState: vnode.state.queryChartsState,
                                                            }),
                                                        ],
                                                    );
                                                }
                                            } else {
                                                const timestamps = [];
                                                const values = [];

                                                resultData.forEach((item, idx) => {
                                                    if (
                                                        item.values &&
                                                        Array.isArray(item.values)
                                                    ) {
                                                        item.values.forEach(([timestamp, value]) => {
                                                            timestamps.push(timestamp);
                                                            values.push(parseFloat(value));
                                                        });
                                                    } else if (
                                                        item.value &&
                                                        Array.isArray(item.value) &&
                                                        item.value.length === 2
                                                    ) {
                                                        timestamps.push(item.value[0]);
                                                        values.push(parseFloat(item.value[1]));
                                                    }
                                                });

                                                if (timestamps.length > 0) {
                                                    const chartData = [timestamps, values];
                                                    const chartKey = `query-chart-line-${vnode.state.query}`;

                                                    const chartSpec = {
                                                        opts: {
                                                            id: chartKey,
                                                            title: 'Query Result',
                                                            style: 'line',
                                                        },
                                                        data: chartData,
                                                    };

                                                    return m(
                                                        'div.query-chart',
                                                        { key: chartKey },
                                                        [
                                                            m(Chart, {
                                                                spec: chartSpec,
                                                                chartsState: vnode.state.queryChartsState,
                                                            }),
                                                        ],
                                                    );
                                                }
                                            }
                                            return null;
                                        })(),
                                    ]
                                  : m('p', 'No data returned'),
                          ])
                        : m(
                              'div.error-message',
                              'Query failed: ' +
                                  (vnode.state.result.error || 'Unknown error'),
                          ),
                ]),

            m('div.example-queries', [
                m('h3', 'Example Queries'),
                m('ul', [
                    m(
                        'li',
                        m(
                            'code',
                            {
                                onclick: () => {
                                    vnode.state.query = 'irate(requests_sent[5m])';
                                    vnode.state.executeQuery();
                                },
                            },
                            'irate(requests_sent[5m])',
                        ),
                        ' - Request rate',
                    ),
                    m(
                        'li',
                        m(
                            'code',
                            {
                                onclick: () => {
                                    vnode.state.query = 'irate(cache_hits[5m])';
                                    vnode.state.executeQuery();
                                },
                            },
                            'irate(cache_hits[5m])',
                        ),
                        ' - Cache hit rate',
                    ),
                    m(
                        'li',
                        m(
                            'code',
                            {
                                onclick: () => {
                                    vnode.state.query =
                                        'irate(cache_hits[5m]) / (irate(cache_hits[5m]) + irate(cache_misses[5m]))';
                                    vnode.state.executeQuery();
                                },
                            },
                            'irate(cache_hits[5m]) / (irate(cache_hits[5m]) + irate(cache_misses[5m]))',
                        ),
                        ' - Cache hit ratio',
                    ),
                    m(
                        'li',
                        m(
                            'code',
                            {
                                onclick: () => {
                                    vnode.state.query =
                                        'histogram_percentiles([0.5, 0.9, 0.99], response_latency)';
                                    vnode.state.executeQuery();
                                },
                            },
                            'histogram_percentiles([0.5, 0.9, 0.99], response_latency)',
                        ),
                        ' - Response latency percentiles',
                    ),
                    m(
                        'li',
                        m(
                            'code',
                            {
                                onclick: () => {
                                    vnode.state.query = 'connections_active';
                                    vnode.state.executeQuery();
                                },
                            },
                            'connections_active',
                        ),
                        ' - Active connections',
                    ),
                    m(
                        'li',
                        m(
                            'code',
                            {
                                onclick: () => {
                                    vnode.state.query = 'irate(bytes_tx[5m]) * 8';
                                    vnode.state.executeQuery();
                                },
                            },
                            'irate(bytes_tx[5m]) * 8',
                        ),
                        ' - TX throughput (bits/sec)',
                    ),
                ]),
            ]),
        ]);
    },
};

// Section-level heatmap mode state
// Key: section route, Value: { enabled: boolean, heatmapData: Map<chartId, data>, loading: boolean }
const sectionHeatmapState = new Map();

// Helper function to check if a section has histogram charts
const sectionHasHistogramCharts = (groups) => {
    if (!groups) return false;
    return groups.some(group =>
        group.plots && group.plots.some(plot =>
            plot.promql_query && plot.promql_query.includes('histogram_percentiles')
        )
    );
};

// Fetch heatmap data for all histogram charts in a section
const fetchSectionHeatmapData = async (sectionRoute, groups) => {
    const state = sectionHeatmapState.get(sectionRoute) || { enabled: false, heatmapData: new Map(), loading: false };
    state.loading = true;
    sectionHeatmapState.set(sectionRoute, state);
    m.redraw();

    const heatmapData = new Map();

    for (const group of groups || []) {
        for (const plot of group.plots || []) {
            if (plot.promql_query && plot.promql_query.includes('histogram_percentiles')) {
                // Convert histogram_percentiles query to histogram_heatmap query
                const match = plot.promql_query.match(/histogram_percentiles\s*\(\s*\[[^\]]*\]\s*,\s*(.+)\)$/);
                if (!match) continue;

                const metricSelector = match[1].trim();
                const heatmapQuery = `histogram_heatmap(${metricSelector})`;

                try {
                    const result = await executePromQLRangeQuery(heatmapQuery);

                    if (result.status === 'success' && result.data && result.data.resultType === 'histogram_heatmap') {
                        const heatmapResult = result.data.result;
                        console.log('[fetchHeatmap] timestamps range:',
                            heatmapResult.timestamps[0], 'to',
                            heatmapResult.timestamps[heatmapResult.timestamps.length - 1],
                            '(', heatmapResult.timestamps.length, 'points)');
                        console.log('[fetchHeatmap] data points:', heatmapResult.data.length);
                        heatmapData.set(plot.opts.id, {
                            time_data: heatmapResult.timestamps,
                            bucket_bounds: heatmapResult.bucket_bounds,
                            data: heatmapResult.data.map(([timeIdx, bucketIdx, count]) => [timeIdx, bucketIdx, count]),
                            min_value: heatmapResult.min_value,
                            max_value: heatmapResult.max_value,
                        });
                    }
                } catch (error) {
                    console.error('Failed to fetch histogram heatmap:', error);
                }
            }
        }
    }

    state.heatmapData = heatmapData;
    state.loading = false;
    sectionHeatmapState.set(sectionRoute, state);
    m.redraw();
};

const SectionContent = {
    view({ attrs }) {
        if (attrs.section.name === 'Query Explorer') {
            return m('div#section-content', [m(QueryExplorer)]);
        }

        const sectionRoute = attrs.section.route;
        const hasHistogramCharts = sectionHasHistogramCharts(attrs.groups);
        const heatmapState = sectionHeatmapState.get(sectionRoute) || { enabled: false, heatmapData: new Map(), loading: false };

        // Toggle handler for heatmap mode
        const toggleHeatmapMode = async () => {
            const newEnabled = !heatmapState.enabled;
            const newState = { ...heatmapState, enabled: newEnabled };
            sectionHeatmapState.set(sectionRoute, newState);

            if (newEnabled && heatmapState.heatmapData.size === 0) {
                // Fetch heatmap data for all histogram charts
                await fetchSectionHeatmapData(sectionRoute, attrs.groups);
            } else {
                m.redraw();
            }
        };

        // Heatmap toggle button (shown when section has histogram charts)
        const heatmapToggle = hasHistogramCharts ? m('button.heatmap-mode-toggle', {
            onclick: toggleHeatmapMode,
            disabled: heatmapState.loading,
        }, heatmapState.loading ? 'Loading...' : (heatmapState.enabled ? 'Show Percentiles' : 'Show Heatmaps')) : null;

        return m('div#section-content', [
            m('div.section-header-row', [
                m('div.zoom-instructions', 'Drag to zoom \u00B7 Scroll to zoom \u00B7 Double-click to reset'),
                heatmapToggle,
            ]),
            m('div.section-title', attrs.section.name),
            m(
                'div#groups',
                attrs.groups.map((group) => m(Group, { ...group, sectionRoute })),
            ),
        ]);
    },
};

// Group component
const Group = {
    view({ attrs }) {
        const sectionRoute = attrs.sectionRoute;
        const heatmapState = sectionHeatmapState.get(sectionRoute);
        const isHeatmapMode = heatmapState?.enabled && !heatmapState?.loading;

        return m(
            'div.group',
            {
                id: attrs.id,
            },
            [
                m('h2', `${attrs.name}`),
                m(
                    'div.charts',
                    attrs.plots.map((spec) => {
                        // Check if this is a histogram chart and we're in heatmap mode
                        const isHistogramChart = spec.promql_query && spec.promql_query.includes('histogram_percentiles');

                        if (isHistogramChart && isHeatmapMode && heatmapState?.heatmapData?.has(spec.opts.id)) {
                            // Create heatmap spec from the fetched data
                            const heatmapData = heatmapState.heatmapData.get(spec.opts.id);
                            const heatmapSpec = {
                                ...spec,
                                opts: {
                                    ...spec.opts,
                                    style: 'histogram_heatmap',
                                },
                                time_data: heatmapData.time_data,
                                bucket_bounds: heatmapData.bucket_bounds,
                                data: heatmapData.data,
                                min_value: heatmapData.min_value,
                                max_value: heatmapData.max_value,
                            };
                            return m(Chart, { spec: heatmapSpec, chartsState });
                        }

                        return m(Chart, { spec, chartsState });
                    }),
                ),
            ],
        );
    },
};

// Application state management (chartsState is defined at the top of the file)

const sectionResponseCache = {};

// Execute a PromQL range query
const executePromQLRangeQuery = async (query) => {
    const metadataResponse = await m.request({
        method: 'GET',
        url: '/api/v1/metadata',
        withCredentials: true,
    });

    if (metadataResponse.status !== 'success') {
        throw new Error('Failed to get metadata');
    }

    const minTime = metadataResponse.data.minTime;
    const maxTime = metadataResponse.data.maxTime;
    const duration = maxTime - minTime;

    const windowDuration = Math.min(3600, duration);
    const start = Math.max(minTime, maxTime - windowDuration);
    const step = Math.max(1, Math.floor(windowDuration / 100));

    const url = `/api/v1/query_range?query=${encodeURIComponent(query)}&start=${start}&end=${maxTime}&step=${step}`;

    return m.request({
        method: 'GET',
        url,
        withCredentials: true,
    });
};

// Process dashboard data and execute PromQL queries where needed
const processDashboardData = async (data) => {
    for (const group of data.groups || []) {
        for (const plot of group.plots || []) {
            if (plot.promql_query) {
                try {
                    const result = await executePromQLRangeQuery(plot.promql_query);

                    if (
                        result.status === 'success' &&
                        result.data &&
                        result.data.result &&
                        result.data.result.length > 0
                    ) {
                        const hasMultipleSeries =
                            result.data.result.length > 1 ||
                            (plot.opts &&
                                (plot.opts.style === 'multi' ||
                                    plot.opts.style === 'heatmap'));

                        if (hasMultipleSeries) {
                            if (plot.opts && plot.opts.style === 'heatmap') {
                                const heatmapData = [];
                                const timeSet = new Set();

                                result.data.result.forEach((item) => {
                                    if (item.values && Array.isArray(item.values)) {
                                        item.values.forEach(([timestamp, _]) => {
                                            timeSet.add(timestamp);
                                        });
                                    }
                                });

                                const timestamps = Array.from(timeSet).sort((a, b) => a - b);
                                const timestampToIndex = new Map();
                                timestamps.forEach((ts, idx) => {
                                    timestampToIndex.set(ts, idx);
                                });

                                result.data.result.forEach((item, idx) => {
                                    if (item.values && Array.isArray(item.values)) {
                                        let yId = idx;
                                        if (item.metric && item.metric.id) {
                                            yId = parseInt(item.metric.id);
                                        }

                                        item.values.forEach(([timestamp, value]) => {
                                            const timeIndex = timestampToIndex.get(timestamp);
                                            heatmapData.push([
                                                timeIndex,
                                                yId,
                                                parseFloat(value),
                                            ]);
                                        });
                                    }
                                });

                                let minValue = Infinity;
                                let maxValue = -Infinity;
                                heatmapData.forEach(([_, __, value]) => {
                                    minValue = Math.min(minValue, value);
                                    maxValue = Math.max(maxValue, value);
                                });

                                plot.data = heatmapData;
                                plot.time_data = timestamps;
                                plot.min_value = minValue;
                                plot.max_value = maxValue;
                            } else {
                                const allData = [];
                                const seriesNames = [];
                                let timestamps = null;

                                result.data.result.forEach((item, idx) => {
                                    if (item.values && Array.isArray(item.values)) {
                                        let seriesName = 'Series ' + (idx + 1);
                                        if (item.metric) {
                                            for (const [key, value] of Object.entries(item.metric)) {
                                                if (key !== '__name__') {
                                                    seriesName = value;
                                                    break;
                                                }
                                            }
                                        }

                                        if (item.values.length > 0) {
                                            seriesNames.push(seriesName);

                                            if (!timestamps) {
                                                timestamps = item.values.map(([ts, _]) => ts);
                                                allData.push(timestamps);
                                            }

                                            const values = item.values.map(([_, val]) =>
                                                parseFloat(val),
                                            );
                                            allData.push(values);
                                        }
                                    }
                                });

                                if (allData.length > 1) {
                                    plot.data = allData;
                                    plot.series_names = seriesNames;
                                } else {
                                    plot.data = [];
                                }
                            }
                        } else {
                            const sample = result.data.result[0];
                            if (sample.values && Array.isArray(sample.values)) {
                                const timestamps = sample.values.map(([ts, _]) => ts);
                                const values = sample.values.map(([_, val]) =>
                                    parseFloat(val),
                                );
                                plot.data = [timestamps, values];
                            } else {
                                plot.data = [];
                            }
                        }
                    } else {
                        console.warn(
                            `Empty or unsuccessful result for query "${plot.promql_query}":`,
                            result,
                        );
                        plot.data = [];
                    }
                } catch (error) {
                    console.error(
                        `Failed to execute PromQL query "${plot.promql_query}":`,
                        error,
                    );
                    plot.data = [];
                }
            }
        }
    }
    return data;
};

// Fetch data for a section and cache it.
const preloadSection = async (section) => {
    if (sectionResponseCache[section]) {
        return Promise.resolve();
    }

    const url = `/data/${section}.json`;
    console.time(`Preload ${url}`);
    const data = await m.request({
        method: 'GET',
        url,
        withCredentials: true,
    });

    const processedData = await processDashboardData(data);
    console.timeEnd(`Preload ${url}`);
    sectionResponseCache[section] = processedData;
};

// Track active preloading to allow cancellation
let preloadingTimeout = null;
let activePreloadPromise = null;

// Preload data for all sections in the background.
const preloadSections = (allSections) => {
    if (preloadingTimeout) {
        clearTimeout(preloadingTimeout);
        preloadingTimeout = null;
    }

    preloadingTimeout = setTimeout(() => {
        const sectionsToPreload = allSections
            .filter((section) => !sectionResponseCache[section.route])
            .map((section) => section.route.substring(1));

        const preloadNext = () => {
            if (sectionsToPreload.length === 0) {
                preloadingTimeout = null;
                return;
            }

            const nextSection = sectionsToPreload.shift();
            activePreloadPromise = preloadSection(nextSection)
                .then(() => {
                    activePreloadPromise = null;
                    if (window.requestIdleCallback) {
                        window.requestIdleCallback(preloadNext, { timeout: 5000 });
                    } else {
                        preloadingTimeout = setTimeout(preloadNext, 500);
                    }
                })
                .catch(() => {
                    activePreloadPromise = null;
                    if (window.requestIdleCallback) {
                        window.requestIdleCallback(preloadNext, { timeout: 5000 });
                    } else {
                        preloadingTimeout = setTimeout(preloadNext, 500);
                    }
                });
        };

        if (window.requestIdleCallback) {
            window.requestIdleCallback(preloadNext, { timeout: 5000 });
        } else {
            preloadingTimeout = setTimeout(preloadNext, 1000);
        }
    }, 5000);
};

// Main application entry point
m.route.prefix = '';
m.route(document.body, '/overview', {
    '/:section': {
        onmatch(params, requestedPath) {
            if (m.route.get() === requestedPath) {
                return new Promise(function () {});
            }

            if (requestedPath !== m.route.get()) {
                chartsState.clear();
                window.scrollTo(0, 0);

                if (preloadingTimeout) {
                    clearTimeout(preloadingTimeout);
                    preloadingTimeout = null;
                }
            }

            if (sectionResponseCache[params.section]) {
                const data = sectionResponseCache[params.section];
                const activeSection = data.sections.find(
                    (section) => section.route === requestedPath,
                );

                // Ensure time info is set for cached data (async, non-blocking)
                if (data.interval && !chartsState.interval) {
                    m.request({
                        method: 'GET',
                        url: '/api/v1/metadata',
                        withCredentials: true,
                    }).then(metadataResponse => {
                        if (metadataResponse.status === 'success' && metadataResponse.data) {
                            chartsState.setTimeInfo(
                                data.interval,
                                metadataResponse.data.minTime,
                                metadataResponse.data.maxTime
                            );
                        }
                    }).catch(() => {});
                }

                return {
                    view() {
                        return m(Main, {
                            ...data,
                            activeSection,
                        });
                    },
                };
            }

            const url = `/data/${params.section}.json`;
            console.time(`Load ${url}`);
            return m
                .request({
                    method: 'GET',
                    url,
                    withCredentials: true,
                })
                .then(async (data) => {
                    console.timeEnd(`Load ${url}`);

                    const processedData = await processDashboardData(data);
                    sectionResponseCache[params.section] = processedData;
                    const activeSection = processedData.sections.find(
                        (section) => section.route === requestedPath,
                    );

                    // Set time info for minimum zoom calculation
                    if (processedData.interval) {
                        try {
                            const metadataResponse = await m.request({
                                method: 'GET',
                                url: '/api/v1/metadata',
                                withCredentials: true,
                            });
                            if (metadataResponse.status === 'success' && metadataResponse.data) {
                                chartsState.setTimeInfo(
                                    processedData.interval,
                                    metadataResponse.data.minTime,
                                    metadataResponse.data.maxTime
                                );
                            }
                        } catch (e) {
                            console.warn('Failed to fetch metadata for zoom limits:', e);
                        }
                    }

                    preloadSections(processedData.sections);

                    return {
                        view() {
                            return m(Main, {
                                ...processedData,
                                activeSection,
                            });
                        },
                    };
                });
        },
    },
});
