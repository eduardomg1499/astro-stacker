
// --- INDEPENDENT ACTIVATION MODAL LOGIC ---

function showActivationModal() {
    const modal = document.getElementById("activation-modal");
    const closeBtn = document.getElementById("activation-modal-close");
    const btnActivate = document.getElementById("btn-manual-activate");
    const input = document.getElementById("activation-key-input");

    // Reset State
    if (input) input.value = "";
    if (document.getElementById("activation-loading")) document.getElementById("activation-loading").style.display = "none";
    if (btnActivate) btnActivate.style.display = "block";

    if (modal) {
        modal.style.display = "flex";

        // Ensure Close Logic
        if (closeBtn) {
            closeBtn.onclick = closeActivationModal;
        }

        // Click Outside
        modal.onclick = (e) => {
            if (e.target === modal) closeActivationModal();
        };
    }
}

function closeActivationModal() {
    const modal = document.getElementById("activation-modal");
    if (modal) modal.style.display = "none";
}

async function handleManualActivation() {
    const input = document.getElementById("activation-key-input");
    const btn = document.getElementById("btn-manual-activate");
    const loader = document.getElementById("activation-loading");

    const key = input.value.trim();
    if (!key || key.length < 5) {
        showCustomAlert("Error", "Ingresa una clave valida.");
        return;
    }

    if (btn) btn.style.display = "none";
    if (loader) loader.style.display = "block";

    try {
        // Generate dynamic device name
        const randomId = Math.random().toString(36).substring(2, 6).toUpperCase();
        const platform = navigator.platform.split(' ')[0] || "PC";
        const dynamicName = `${platform}-${randomId}`;

        await invoke("activate_pro_license", { key: key, deviceName: dynamicName });

        await showCustomAlert(
            "¡Licencia Actualizada!",
            "La licencia se ha activado correctamente."
        );

        await checkLicenseAtStartup();
        refreshSettingsLicenseInfo(); // Refresh settings UI
        closeActivationModal();

    } catch (e) {
        showCustomAlert("Error de Activacion", e);
    } finally {
        if (btn) btn.style.display = "block";
        if (loader) loader.style.display = "none";
    }
}

// Bind Events for Manual Activation
const btnManualActivate = document.getElementById("btn-manual-activate");
if (btnManualActivate) {
    btnManualActivate.addEventListener("click", handleManualActivation);
}
