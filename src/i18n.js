/**
 * Simple I18n Manager for Zenith Astro Stacker
 * Handles loading standard JSON files and updating the DOM
 */

export class I18nManager {
    constructor() {
        this.fallbackLang = 'es';
        const savedLang = localStorage.getItem('app_language');
        const browserLang = navigator.language?.split('-')[0];
        this.currentLang = this.normalizeLang(savedLang || browserLang || this.fallbackLang);
        this.translations = {};
    }

    normalizeLang(lang) {
        const normalized = String(lang || '').toLowerCase().split('-')[0];
        return ['es', 'en', 'it', 'fr'].includes(normalized) ? normalized : 'es';
    }

    async init() {
        // Load default language first (always available as fallback)
        await this.loadLanguage(this.fallbackLang);

        // Load user preference if different
        if (this.currentLang !== this.fallbackLang) {
            try {
                await this.loadLanguage(this.currentLang);
            } catch (e) {
                console.warn(`Could not load language ${this.currentLang}, falling back to ${this.fallbackLang}`);
                this.currentLang = this.fallbackLang;
            }
        }

        localStorage.setItem('app_language', this.currentLang);
        console.log(`I18n Initialized: ${this.currentLang}`);
        this.translatePage();
    }

    async loadLanguage(lang) {
        lang = this.normalizeLang(lang);
        try {
            // In Tauri/Vite, we can import JSON directly or fetch it
            // Using fetch relative to the public/assets or src logic depending on build
            // For simplicity in this project structure, we assume they are copied to assets or accessible
            // BUT since this is a Vite project, dynamic imports of JSON inside src might need special handling
            // Let's try dynamic import first which is standard in Vite

            // Note: In Vite, dynamic imports with variables can be tricky. 
            // We use a switch or explicit map for known languages to be safe and robust.
            let data;
            if (lang === 'en') data = await import('./locales/en.json');
            else if (lang === 'it') data = await import('./locales/it.json');
            else if (lang === 'fr') data = await import('./locales/fr.json');
            else data = await import('./locales/es.json');

            const loaded = data.default || data;
            if (lang === 'it' || lang === 'fr') {
                const englishModule = await import('./locales/en.json');
                const english = englishModule.default || englishModule;
                this.translations[lang] = this.mergeTranslations(english, loaded);
            } else {
                this.translations[lang] = loaded;
            }
        } catch (e) {
            console.error(`Failed to load language: ${lang}`, e);
            throw e;
        }
    }

    mergeTranslations(base, overlay) {
        if (!base || typeof base !== 'object' || Array.isArray(base)) return overlay;
        const output = { ...base };
        Object.entries(overlay || {}).forEach(([key, value]) => {
            output[key] = value && typeof value === 'object' && !Array.isArray(value)
                ? this.mergeTranslations(base[key] || {}, value)
                : value;
        });
        return output;
    }

    async setLanguage(lang) {
        lang = this.normalizeLang(lang);
        const changed = lang !== this.currentLang;

        await this.loadLanguage(lang);
        this.currentLang = lang;
        localStorage.setItem('app_language', lang);

        if (changed) {
            this.translatePage();
            // Dispatch event for other components only when the visible language changed
            window.dispatchEvent(new CustomEvent('languageChanged', { detail: { lang } }));
        } else {
            this.updateDynamicElements();
        }
    }

    t(key) {
        const keys = key.split('.');
        let value = this.translations[this.currentLang];

        for (const k of keys) {
            if (value && value[k] !== undefined) {
                value = value[k];
            } else {
                // Fallback to default lang
                let fallback = this.translations[this.fallbackLang];
                for (const fk of keys) {
                    if (fallback && fallback[fk] !== undefined) {
                        fallback = fallback[fk];
                    } else {
                        return key; // Return valid key if absolutely nothing found
                    }
                }
                return fallback || key;
            }
        }
        return value;
    }

    translatePage() {
        // Translate content
        const elements = document.querySelectorAll('[data-i18n]');
        elements.forEach(el => {
            const key = el.getAttribute('data-i18n');
            const translation = this.t(key);

            if (translation) {
                if (el.tagName === 'INPUT' && el.type === 'text' && (el.placeholder !== undefined)) {
                    el.placeholder = translation;
                } else if (el.tagName === 'OPTION') {
                    // Strip HTML tags when translating native <option> tags since they only support plain text
                    el.textContent = translation.replace(/<[^>]+>/g, '').trim();
                } else {
                    if (translation.includes('<')) {
                        el.innerHTML = translation;
                    } else {
                        el.textContent = translation;
                    }
                }
            }
        });

        // Translate specific attributes (like title)
        const attrElements = document.querySelectorAll('[data-i18n-title]');
        attrElements.forEach(el => {
            const key = el.getAttribute('data-i18n-title');
            const translation = this.t(key);
            if (translation) {
                el.setAttribute('title', translation);
            }
        });

        const ariaElements = document.querySelectorAll('[data-i18n-aria-label]');
        ariaElements.forEach(el => {
            const key = el.getAttribute('data-i18n-aria-label');
            const translation = this.t(key);
            if (translation && translation !== key) {
                el.setAttribute('aria-label', translation);
            }
        });

        const placeholderElements = document.querySelectorAll('[data-i18n-placeholder]');
        placeholderElements.forEach(el => {
            const key = el.getAttribute('data-i18n-placeholder');
            const translation = this.t(key);
            if (translation && translation !== key) {
                el.setAttribute('placeholder', translation);
            }
        });

        this.updateDynamicElements();
    }

    updateDynamicElements() {
        // Helper hook for things that aren't simple data-i18n attributes
        // e.g. updating the html lang attribute
        document.documentElement.lang = this.currentLang;
    }
}

export const i18n = new I18nManager();
