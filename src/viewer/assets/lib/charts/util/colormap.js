/**
 * ColorMapper provides consistent color assignment for series across all charts
 * using a deterministic hash function to assign colors based on series names.
 */
export class ColorMapper {
    constructor() {
        // Store color assignments
        this.colorMap = new Map();
        // Track which series are selected (and thus need colors)
        this.selectedSeries = new Set();
        // Track the order colors were assigned
        this.colorAssignmentOrder = [];

        this.colorPalette = [
            '#BF4040',
            '#D66C5C',
            '#AD451F',
            '#BF7340',
            '#D69D5C',
            '#AD7E1F',
            '#BFA640',
            '#D6CE5C',
            '#A4AD1F',
            '#A6BF40',
            '#ADD65C',
            '#6BAD1F',
            '#73BF40',
            '#7CD65C',
            '#32AD1F',
            '#40BF40',
            '#5CD66C',
            '#1FAD45',
            '#40BF73',
            '#5CD69D',
            '#1FAD7E',
            '#40BFA6',
            '#5CD6CE',
            '#1FA4AD',
            '#40A6BF',
            '#5CADD6',
            '#1F6BAD',
            '#4073BF',
            '#5C7CD6',
            '#1F32AD',
            '#4040BF',
            '#6C5CD6',
            '#451FAD',
            '#7340BF',
            '#9D5CD6',
            '#7E1FAD',
            '#A640BF',
            '#CE5CD6',
            '#AD1FA4',
            '#BF40A6',
            '#D65CAD',
            '#AD1F6B',
            '#BF4073',
            '#D65C7C',
            '#AD1F32',
        ]

        // Always use gray for "Other" category
        this.otherColor = '#666666';
    }

    /**
     * Get a simple hash value from a string
     * @param {string} str - The string to hash
     * @returns {number} A numeric hash value
     */
    stringToHash(str) {
        let hash = 0;
        for (let i = 0; i < str.length; i++) {
            const char = str.charCodeAt(i);
            hash = ((hash << 5) - hash) + char;
            hash = hash & hash; // Convert to 32bit integer
        }
        return Math.abs(hash);
    }

    /**
     * Get the color for a specific series, using a deterministic mapping based on the name.
     * @param {string} seriesName - The name of the series
     * @returns {string} The color code for this series
     */
    getColorByName(seriesName) {
        if (seriesName === "Other") {
            return this.otherColor;
        }

        if (this.colorMap.has(seriesName)) {
            return this.colorMap.get(seriesName);
        }

        const hash = this.stringToHash(seriesName);
        const colorIndex = hash % this.colorPalette.length;
        const color = this.colorPalette[colorIndex];

        this.colorMap.set(seriesName, color);
        return color;
    }

    /**
     * Get the nth color in the fixed color palette.
     * @param {number} index - The index to get a color for
     * @returns {string} The color code for this index
     */
    getColorByIndex(index) {
        const STARTING_INDEX = 2;
        const JUMP_DISTANCE = 12;
        return this.colorPalette[(STARTING_INDEX + index * JUMP_DISTANCE) % this.colorPalette.length];
    }

    /**
     * Get colors for an array of series names
     * @param {string[]} seriesNames - Array of series names
     * @returns {string[]} Array of color codes in the same order
     */
    getColors(seriesNames) {
        return seriesNames.map((name) => this.getColorByName(name));
    }

    /**
     * Get color for the "Other" category
     * @returns {string} The color for the "Other" category
     */
    getOtherColor() {
        return this.otherColor;
    }

    /**
     * Clear all color mappings
     */
    clear() {
        this.colorMap.clear();
        this.selectedSeries.clear();
        this.colorAssignmentOrder = [];
    }

    /**
     * Add a series to the selected set and assign it a color
     * @param {string} seriesName - The name of the series to select
     */
    selectCgroup(seriesName) {
        if (!this.selectedSeries.has(seriesName) && seriesName !== 'Other') {
            this.selectedSeries.add(seriesName);

            if (!this.colorMap.has(seriesName)) {
                const nextIndex = this.colorAssignmentOrder.length;
                const color = this.getColorByIndex(nextIndex);
                this.colorMap.set(seriesName, color);
                this.colorAssignmentOrder.push(seriesName);
            }
        }
    }

    /**
     * Remove a series from the selected set and free its color
     * @param {string} seriesName - The name of the series to deselect
     */
    deselectCgroup(seriesName) {
        if (this.selectedSeries.has(seriesName)) {
            this.selectedSeries.delete(seriesName);

            const index = this.colorAssignmentOrder.indexOf(seriesName);
            if (index > -1) {
                this.colorAssignmentOrder.splice(index, 1);
            }

            this.colorMap.delete(seriesName);
            this.reassignColors();
        }
    }

    /**
     * Reassign colors to all selected series based on their order
     */
    reassignColors() {
        this.colorMap.clear();
        this.colorAssignmentOrder.forEach((seriesName, index) => {
            const color = this.getColorByIndex(index);
            this.colorMap.set(seriesName, color);
        });
    }

    /**
     * Get the color for a selected series
     * @param {string} seriesName - The name of the series
     * @returns {string|null} The color code for this series, or null if not selected
     */
    getSelectedCgroupColor(seriesName) {
        if (seriesName === 'Other') {
            return this.otherColor;
        }

        if (this.selectedSeries.has(seriesName)) {
            return this.colorMap.get(seriesName) || null;
        }

        return null;
    }
}

// Create a singleton instance to share across the application
const globalColorMapper = new ColorMapper();
export default globalColorMapper;
